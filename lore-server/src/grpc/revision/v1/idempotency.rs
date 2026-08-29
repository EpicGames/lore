// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Durable request-id reservations for server-authored revisions.

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_storage::KeyType;
use lore_storage::options::ReadOptions;
use lore_storage::options::WriteOptions;
use serde::Deserialize;
use serde::Serialize;
use tonic::Status;

const PENDING_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    request_digest: Hash,
    created_at_epoch_seconds: u64,
    revision: Hash,
    revision_number: u64,
}

impl Record {
    fn pending(request_digest: Hash) -> Self {
        Self {
            request_digest,
            created_at_epoch_seconds: now(),
            revision: Hash::default(),
            revision_number: 0,
        }
    }

    fn is_complete(&self) -> bool {
        !self.revision.is_zero()
    }

    fn is_expired(&self) -> bool {
        now().saturating_sub(self.created_at_epoch_seconds) >= PENDING_TTL.as_secs()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) struct Reservation {
    operation: &'static str,
    key: Hash,
    record_hash: Hash,
    request_id: Context,
    request_digest: Hash,
}

pub(crate) enum Start {
    Completed {
        revision: Hash,
        revision_number: u64,
    },
    Reserved(Reservation),
}

fn key(operation: &str, branch: BranchId, request_id: Context) -> Hash {
    let mut bytes = Vec::with_capacity(operation.len() + 1 + 16 + 16);
    bytes.extend_from_slice(operation.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(branch.data());
    bytes.extend_from_slice(request_id.data());
    Hash::hash_buffer(&bytes)
}

async fn store_record(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    repository: Partition,
    request_id: Context,
    record: &Record,
) -> Result<Hash, Status> {
    let bytes = serde_json::to_vec(record)
        .map_err(|error| Status::internal(format!("serialize idempotency record: {error}")))?;
    let address = lore_storage::write_content(
        immutable_store,
        repository,
        request_id,
        Bytes::from(bytes),
        WriteOptions::default().with_local_cache_priority(),
        None,
        None,
        None,
    )
    .await
    .map_err(|error| Status::internal(format!("store idempotency record: {error}")))?;
    Ok(address.address.hash)
}

async fn load_record(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    repository: Partition,
    request_id: Context,
    record_hash: Hash,
) -> Result<Record, Status> {
    let (_, bytes) = lore_storage::read(
        immutable_store,
        repository,
        Address {
            hash: record_hash,
            context: request_id,
        },
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .map_err(|error| Status::internal(format!("load idempotency record: {error}")))?;
    serde_json::from_slice(bytes.as_ref())
        .map_err(|error| Status::internal(format!("decode idempotency record: {error}")))
}

pub(crate) async fn begin(
    operation: &'static str,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: Partition,
    branch: BranchId,
    request_id: Context,
    request_digest: Hash,
) -> Result<Start, Status> {
    let key = key(operation, branch, request_id);
    for _ in 0..4 {
        let current = match mutable_store
            .clone()
            .load(repository, key, KeyType::Untyped)
            .await
        {
            Ok(hash) => hash,
            Err(error) if error.is_address_not_found() => Hash::default(),
            Err(error) => {
                return Err(Status::internal(format!(
                    "load {operation} idempotency pointer: {error}"
                )));
            }
        };

        if !current.is_zero() {
            let record =
                load_record(immutable_store.clone(), repository, request_id, current).await?;
            if record.request_digest != request_digest {
                return Err(Status::already_exists(
                    "request_id was already used with different request content",
                ));
            }
            if record.is_complete() {
                return Ok(Start::Completed {
                    revision: record.revision,
                    revision_number: record.revision_number,
                });
            }
            if !record.is_expired() {
                return Err(Status::aborted(format!(
                    "an identical {operation} request is still in progress; retry later"
                )));
            }
        }

        let pending_hash = store_record(
            immutable_store.clone(),
            repository,
            request_id,
            &Record::pending(request_digest),
        )
        .await?;
        let observed = mutable_store
            .clone()
            .compare_and_swap(repository, key, current, pending_hash, KeyType::Untyped)
            .await
            .map_err(|error| {
                Status::internal(format!("reserve {operation} request_id: {error}"))
            })?;
        if observed == current {
            return Ok(Start::Reserved(Reservation {
                operation,
                key,
                record_hash: pending_hash,
                request_id,
                request_digest,
            }));
        }
    }
    Err(Status::aborted(format!(
        "{operation} request_id reservation changed concurrently; retry"
    )))
}

pub(crate) async fn release(
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: Partition,
    reservation: &Reservation,
) {
    let _ = mutable_store
        .compare_and_swap(
            repository,
            reservation.key,
            reservation.record_hash,
            Hash::default(),
            KeyType::Untyped,
        )
        .await;
}

pub(crate) async fn complete(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: Partition,
    reservation: &Reservation,
    revision: Hash,
    revision_number: u64,
) -> Result<(), Status> {
    let completed_hash = store_record(
        immutable_store,
        repository,
        reservation.request_id,
        &Record {
            request_digest: reservation.request_digest,
            created_at_epoch_seconds: now(),
            revision,
            revision_number,
        },
    )
    .await?;
    let observed = mutable_store
        .compare_and_swap(
            repository,
            reservation.key,
            reservation.record_hash,
            completed_hash,
            KeyType::Untyped,
        )
        .await
        .map_err(|error| {
            Status::internal(format!(
                "complete {} idempotency record: {error}",
                reservation.operation
            ))
        })?;
    if observed != reservation.record_hash {
        return Err(Status::internal(format!(
            "{} idempotency reservation changed before completion",
            reservation.operation
        )));
    }
    Ok(())
}
