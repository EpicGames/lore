// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use aws_smithy_types::Blob;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use bytes::Bytes;
use lore_base::error::LockNotFound;
use lore_base::error::LockNotOwned;
use lore_base::error::SlowDown;
use lore_base::types::Hash;
use lore_base::types::LockData;
use lore_base::types::LockResource;
use lore_error_set::prelude::*;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::info_span;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::aws_error::AwsError;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbQuery;
use crate::dynamodb::cancellation_reason::interesting_cancellation_reason_filter;
use crate::dynamodb::operation::transact_write_items::TransactWriteItemsError;
use crate::dynamodb::types::AttributeValue;
use crate::dynamodb::types::BatchStatementErrorCodeEnum;
use crate::dynamodb::types::CancellationReason;
use crate::dynamodb::types::Delete;
use crate::dynamodb::types::Put;
use crate::dynamodb::types::ReturnValuesOnConditionCheckFailure;
use crate::dynamodb::types::TransactWriteItem;

// If there is no code to an AWS error, rather than having nothing (which won't get added to a trace event)
// fallback to something so we can at least aggregate on a common term and also be sure the events are working
const TRACING_AWS_NO_CODE_FALLBACK: &str = "<no code>";

pub const BRANCH_KEY: &str = "branch";
pub const DESC_KEY: &str = "description";
pub const HASH_KEY: &str = "hash";
pub const OWNER_KEY: &str = "ownerId";
pub const OWNER_REPO_BRANCH_GSI: &str = "owner-repo-branch";
pub const REPO_BRANCH_DESC_GSI: &str = "repo-branch-description";
pub const REPO_BRANCH_GSI: &str = "repo-branch";
pub const REPO_BRANCH_KEY: &str = "repositoryBranch";
pub const REPO_KEY: &str = "repository";

pub struct DynamoDbLockStore {
    dynamodb: DynamoDb,
    table_name: Arc<str>,
}

impl DynamoDbLockStore {
    pub fn new(dynamodb: DynamoDb, table_name: impl Into<String>) -> Self {
        Self {
            dynamodb,
            table_name: Arc::from(table_name.into()),
        }
    }
}

#[derive(Hash, Eq, PartialEq, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LockEntryKey {
    pub repository: RepositoryId,
    pub branch: BranchId,
    pub resource_hash: Hash,
}

#[derive(Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockEntry {
    pub description: String,
    pub branch: BranchId,
    pub hash: Hash,
    pub owner_id: String,
    pub repository: RepositoryId,
    pub repository_branch: Bytes,
    pub timestamp: String,
}

impl std::fmt::Debug for LockEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockEntry")
            .field("description", &self.description)
            .field("branch", &self.branch)
            .field("hash", &self.hash)
            .field("owner_id", &self.owner_id)
            .field("repository", &self.repository)
            .field("repository_branch", &hex::encode(&self.repository_branch))
            .field("timestamp", &self.timestamp)
            .finish()
    }
}

impl TryFrom<LockEntry> for LockData {
    type Error = LockError;
    fn try_from(value: LockEntry) -> Result<Self, Self::Error> {
        let timestamp = chrono::DateTime::parse_from_rfc3339(value.timestamp.as_str())
            .internal("failed to parse timestamp")?;
        let resource = LockResource {
            branch: value.branch,
            hash: value.hash,
            description: value.description,
        };
        Ok(Self {
            resource,
            owner: value.owner_id,
            locked_at: timestamp.timestamp_millis() as u64,
        })
    }
}

#[derive(Clone, Deserialize, Hash, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockKey {
    pub hash: Hash,
    pub repository_branch: Bytes,
}

impl std::fmt::Debug for LockKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockKey")
            .field("hash", &self.hash)
            .field("repository_branch", &hex::encode(&self.repository_branch))
            .finish()
    }
}

impl DynamoDbQuery for LockQuery {
    fn index_name(&self) -> Option<String> {
        match self {
            Self::Owner(_) | Self::OwnerRepository(_, _) | Self::OwnerRepositoryBranch(_, _, _) => {
                Some(OWNER_REPO_BRANCH_GSI.to_string())
            }
            Self::Repository(_) | Self::RepositoryBranch(_, _) => Some(REPO_BRANCH_GSI.to_string()),
            Self::RepositoryBranchDescription(_, _, _) => Some(REPO_BRANCH_DESC_GSI.to_string()),
            _ => None,
        }
    }

    fn key_condition_expression(&self) -> &str {
        match self {
            Self::Hash(_) => "#pk = :hash",
            Self::HashRepository(_, _) => "#pk = :hash and begins_with(#sk, :repo)",
            Self::HashRepositoryBranch(_, _, _) => "#pk = :hash and #sk = :repoBranch",
            Self::Owner(_) => "#pk = :owner",
            Self::OwnerRepository(_, _) => "#pk = :owner and begins_with(#sk, :repo)",
            Self::OwnerRepositoryBranch(_, _, _) => "#pk = :owner and #sk = :repoBranch",
            Self::Repository(_) => "#pk = :repo",
            Self::RepositoryBranch(_, _) => "#pk = :repo and #sk = :branch",
            Self::RepositoryBranchDescription(_, _, _) => {
                "#pk = :repoBranch and #sk = :description"
            }
        }
    }

    fn expression_attribute_names(&self) -> HashMap<String, String> {
        match self {
            Self::Hash(_) => HashMap::from([("#pk".to_string(), HASH_KEY.to_string())]),
            Self::HashRepository(_, _) | Self::HashRepositoryBranch(_, _, _) => HashMap::from([
                ("#pk".to_string(), HASH_KEY.to_string()),
                ("#sk".to_string(), REPO_BRANCH_KEY.to_string()),
            ]),
            Self::Owner(_) => HashMap::from([("#pk".to_string(), OWNER_KEY.to_string())]),
            Self::OwnerRepository(_, _) | Self::OwnerRepositoryBranch(_, _, _) => HashMap::from([
                ("#pk".to_string(), OWNER_KEY.to_string()),
                ("#sk".to_string(), REPO_BRANCH_KEY.to_string()),
            ]),
            Self::Repository(_) => HashMap::from([("#pk".to_string(), REPO_KEY.to_string())]),
            Self::RepositoryBranch(_, _) => HashMap::from([
                ("#pk".to_string(), REPO_KEY.to_string()),
                ("#sk".to_string(), BRANCH_KEY.to_string()),
            ]),
            Self::RepositoryBranchDescription(_, _, _) => HashMap::from([
                ("#pk".to_string(), REPO_BRANCH_KEY.to_string()),
                ("#sk".to_string(), DESC_KEY.to_string()),
            ]),
        }
    }

    fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
        match self {
            Self::Hash(hash) => HashMap::from([(
                ":hash".to_string(),
                AttributeValue::B(Blob::new(hash.as_bytes())),
            )]),
            Self::HashRepository(hash, repository) => HashMap::from([
                (
                    ":hash".to_string(),
                    AttributeValue::B(Blob::new(hash.as_bytes())),
                ),
                (
                    ":repo".to_string(),
                    AttributeValue::B(Blob::new(repository.as_bytes())),
                ),
            ]),
            Self::HashRepositoryBranch(hash, repository, branch) => HashMap::from([
                (
                    ":hash".to_string(),
                    AttributeValue::B(Blob::new(hash.as_bytes())),
                ),
                (
                    ":repoBranch".to_string(),
                    AttributeValue::B(Blob::new(
                        repository
                            .data()
                            .iter()
                            .chain(branch.data().iter())
                            .copied()
                            .collect::<Vec<u8>>(),
                    )),
                ),
            ]),
            Self::Owner(owner) => {
                HashMap::from([(":owner".to_string(), AttributeValue::S(owner.clone()))])
            }
            Self::OwnerRepository(owner, repository) => HashMap::from([
                (":owner".to_string(), AttributeValue::S(owner.clone())),
                (
                    ":repo".to_string(),
                    AttributeValue::B(Blob::new(repository.as_bytes())),
                ),
            ]),
            Self::OwnerRepositoryBranch(owner, repository, branch) => HashMap::from([
                (":owner".to_string(), AttributeValue::S(owner.clone())),
                (
                    ":repoBranch".to_string(),
                    AttributeValue::B(Blob::new(
                        repository
                            .data()
                            .iter()
                            .chain(branch.data().iter())
                            .copied()
                            .collect::<Vec<u8>>(),
                    )),
                ),
            ]),
            Self::Repository(repository) => HashMap::from([(
                ":repo".to_string(),
                AttributeValue::B(Blob::new(repository.as_bytes())),
            )]),
            Self::RepositoryBranch(repository, branch) => HashMap::from([
                (
                    ":repo".to_string(),
                    AttributeValue::B(Blob::new(repository.as_bytes())),
                ),
                (
                    ":branch".to_string(),
                    AttributeValue::B(Blob::new(branch.as_bytes())),
                ),
            ]),
            Self::RepositoryBranchDescription(repository, branch, description) => HashMap::from([
                (
                    ":repoBranch".to_string(),
                    AttributeValue::B(Blob::new(
                        repository
                            .data()
                            .iter()
                            .chain(branch.data().iter())
                            .copied()
                            .collect::<Vec<u8>>(),
                    )),
                ),
                (
                    ":description".to_string(),
                    AttributeValue::S(description.clone()),
                ),
            ]),
        }
    }
}

/// Checks to see if the `DynamoDB` request to lock a resource failed due to a conditional check
/// failure, and if so further checks to see if the lock that existed was owned by the same owner
/// trying to acquire the lock. In that scenario, return a `LockEntryKey` that should be removed
/// from the batch of locks to acquire before retrying, otherwise returns an appropriate `LockError`
fn check_lock_already_owned(
    repository: RepositoryId,
    owner_id: &str,
    reason: &CancellationReason,
) -> Result<LockEntryKey, LockError> {
    // Ensure we got a valid reason code and if so, that it was a conditional check failure
    let code = reason.code().map(BatchStatementErrorCodeEnum::from);

    if let Some(BatchStatementErrorCodeEnum::ThrottlingError) = code {
        warn!(
            "LockData resources failed, DynamoDB throughput exceeded: {code:?}, message: {:?}",
            reason.message
        );
        return Err(SlowDown.into());
    }

    code.filter(|error_code| {
        matches!(
            error_code,
            BatchStatementErrorCodeEnum::ConditionalCheckFailed
        )
    })
    .ok_or_else(|| {
        warn!(
            "LockData resources failed, unexpected reason code in Dynamo response: {:?}, message: {:?}",
            reason.code(),
            reason.message()
        );
        LockError::internal("unexpected DynamoDB error code")
    })?;

    if let Some(item) = &reason.item {
        let lock_entry: LockEntry =
            serde_dynamo::from_item(item.clone()).internal("failed to deserialize lock entry")?;

        // TODO: generate metric

        debug!(
            "Verifying existing owner: {} - {}",
            lock_entry.owner_id, owner_id
        );
        if lock_entry.owner_id == owner_id {
            Ok(LockEntryKey {
                repository,
                branch: lock_entry.branch,
                resource_hash: lock_entry.hash,
            })
        } else {
            Err(LockNotOwned.into())
        }
    } else {
        warn!("Reason does not contain a valid item.");
        Err(LockError::internal("cancellation reason missing item"))
    }
}

#[async_trait]
impl LockStore for DynamoDbLockStore {
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::lock_resources", skip_all)]
    async fn lock_resources(
        &self,
        owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let mut locks = HashMap::<LockEntryKey, LockData>::with_capacity(resources.len());
        let mut write_items =
            HashMap::<LockEntryKey, TransactWriteItem>::with_capacity(resources.len());

        // Use the same timestamp for all locks
        let timestamp = chrono::Utc::now();

        for resource in resources {
            let candidate_key = LockEntryKey {
                repository,
                branch: resource.branch,
                resource_hash: resource.hash,
            };

            let lock_entry = LockEntry {
                description: resource.description.clone(),
                branch: resource.branch,
                hash: resource.hash,
                owner_id: owner_id.to_string(),
                repository,
                repository_branch: Bytes::from_owner(
                    [*repository.data(), *resource.branch.data()].concat(),
                ),
                timestamp: timestamp.to_rfc3339(),
            };

            let item =
                serde_dynamo::to_item(&lock_entry).internal("failed to serialize lock entry")?;

            let put_item = Put::builder()
                .table_name(&*self.table_name)
                .set_item(Some(item))
                // Since PutItem will pull up an existing PK + SK combo to begin with,
                // only checking for existence of the PK attr is necessary
                .condition_expression("attribute_not_exists(#pk)")
                .expression_attribute_names("#pk", HASH_KEY)
                .return_values_on_condition_check_failure(
                    ReturnValuesOnConditionCheckFailure::AllOld,
                )
                .build()
                .internal("failed to create put request")?;

            write_items.insert(
                candidate_key.clone(),
                TransactWriteItem::builder().put(put_item).build(),
            );

            locks.insert(
                candidate_key.clone(),
                LockData {
                    resource: resource.clone(),
                    owner: owner_id.to_string(),
                    locked_at: timestamp.timestamp_millis() as u64,
                },
            );
        }

        loop {
            let result = self
                .dynamodb
                .transact_write_items(write_items.values().cloned().collect())
                .await;

            // If the transaction failed because of locks already exist
            // a) check if the owner is the user that issue the lock request, remove it and retry lock
            // b) otherwise the request needs to fail
            match result {
                Ok(_) => break,
                Err(AwsError::AwsSdkError(error)) => {
                    let span = info_span!(
                        "transact_write_items_error",
                        aws_error_code = error.code().unwrap_or(TRACING_AWS_NO_CODE_FALLBACK),
                        resources_length = resources.len()
                    );
                    let _ = span.or_current().enter();

                    match error.as_service_error() {
                        Some(TransactWriteItemsError::TransactionCanceledException(exception)) => {
                            for reason in exception
                                .cancellation_reasons()
                                .iter()
                                .filter(interesting_cancellation_reason_filter)
                            {
                                let candidate_key =
                                    check_lock_already_owned(repository, owner_id, reason)?;

                                write_items.remove(&candidate_key);
                                locks.remove(&candidate_key);
                            }

                            if write_items.is_empty() {
                                debug!("No items to write to DDB");
                                break;
                            }
                        }
                        // It seems that when DynamoDb is throttling TransactWriteItems calls, if the
                        // entire transaction call is throttled (rather than individual parts of the
                        // transaction) we get back an `TransactWriteItemsError::Unhandled` with a code
                        // of `"ThrottlingException"`, rather than a `TransactionCanceledException` with
                        // a reason of `BatchStatementErrorCodeEnum::ThrottlingError`.
                        Some(e) if e.code() == Some("ThrottlingException") => {
                            warn!(
                                "DynamoDB rate limit exceeded while locking {} resources for {owner_id} in repository {repository} {e:?}",
                                resources.len()
                            );
                            return Err(SlowDown.into());
                        }
                        Some(e) => {
                            warn!(
                                "Unexpected transact write items error while locking {} resources for {owner_id} in repository {repository} {e:?}",
                                resources.len()
                            );
                            return Err(LockError::internal(
                                "unexpected transact write items error while locking",
                            ));
                        }
                        None => {
                            warn!("Cannot treat error as service_error: {error:?}");
                            return Err(LockError::internal("cannot treat error as service error"));
                        }
                    }
                }
                Err(error) => {
                    warn!("Unexpected error: {error:?}");
                    return Err(LockError::internal(
                        "unexpected error while locking resources",
                    ));
                }
            }
        }

        Ok(locks.values().cloned().collect())
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::query_locks", skip_all)]
    async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError> {
        let output = self
            .dynamodb
            .query_paginated(&self.table_name, query)
            .await
            .internal("failed to query locks")?;

        let mut locks = vec![];

        if let Some(items) = output.items {
            for item in items {
                let lock_entry: LockEntry =
                    serde_dynamo::from_item(item).internal("failed to deserialize lock entry")?;

                locks.push(LockData::try_from(lock_entry)?);
            }
        }

        Ok(locks)
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::check_locks_status", skip_all)]
    async fn check_locks_status(
        &self,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let deduplicated_resources: HashSet<&LockResource> = HashSet::from_iter(resources);

        if deduplicated_resources.len() != resources.len() {
            let num_duplicates = resources.len() - deduplicated_resources.len();
            debug!(
                num_duplicates,
                "Found duplicate resources when checking lock status.",
            );
        }

        let mut items = Vec::with_capacity(deduplicated_resources.len());

        for resource in deduplicated_resources.iter() {
            let lock_key = LockKey {
                hash: resource.hash,
                repository_branch: Bytes::from_owner(
                    [*repository.data(), *resource.branch.data()].concat(),
                ),
            };
            let item = serde_dynamo::to_item(&lock_key).internal("failed to serialize lock key")?;

            items.push(item);
        }

        let results = self
            .dynamodb
            .batch_get_item(&self.table_name, items, false /* consistent */)
            .await
            .internal("failed to batch get lock items")?;

        let mut output = Vec::with_capacity(results.len());

        for item in results {
            let lock: LockEntry =
                serde_dynamo::from_item(item).internal("failed to deserialize lock entry")?;

            output.push(lock.try_into()?);
        }

        Ok(output)
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::unlock_resources", skip_all)]
    async fn unlock_resources(
        &self,
        owner_id: &str,
        validate_user: bool,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        let len = resources.len();

        let mut resources = resources.to_vec();
        resources.sort();
        resources.dedup();

        if resources.len() != len {
            debug!(
                "Found {} duplicate resources when checking lock status.",
                len - resources.len()
            );
        }

        let mut write_items = Vec::with_capacity(resources.len());

        for resource in &resources {
            let lock_key = LockKey {
                hash: resource.hash,
                repository_branch: Bytes::from_owner(
                    [*repository.data(), *resource.branch.data()].concat(),
                ),
            };

            let key = serde_dynamo::to_item(&lock_key).internal("failed to serialize lock key")?;

            let delete_item = if validate_user {
                Delete::builder()
                    .table_name(&*self.table_name)
                    .set_key(Some(key))
                    .condition_expression("ownerId = :val")
                    .expression_attribute_values(":val", AttributeValue::S(owner_id.to_string()))
                    .return_values_on_condition_check_failure(
                        ReturnValuesOnConditionCheckFailure::AllOld,
                    )
                    .build()
                    .internal("failed to create delete request")?
            } else {
                Delete::builder()
                    .table_name(&*self.table_name)
                    .set_key(Some(key))
                    .build()
                    .internal("failed to create delete request")?
            };

            write_items.push(TransactWriteItem::builder().delete(delete_item).build());
        }

        match self.dynamodb.transact_write_items(write_items).await {
            Ok(_) => Ok(resources),
            Err(AwsError::AwsSdkError(error)) => {
                let span = info_span!(
                    "transact_write_items_error",
                    aws_error_code = error.code().unwrap_or(TRACING_AWS_NO_CODE_FALLBACK),
                    resources_length = resources.len()
                );
                let _ = span.or_current().enter();

                if let Some(TransactWriteItemsError::TransactionCanceledException(exception)) =
                    error.as_service_error()
                {
                    for reason in exception
                        .cancellation_reasons()
                        .iter()
                        .filter(interesting_cancellation_reason_filter)
                    {
                        let reason_code = reason.code().unwrap_or_default();
                        let batch_statement_error = BatchStatementErrorCodeEnum::from(reason_code);
                        if batch_statement_error
                            == BatchStatementErrorCodeEnum::ConditionalCheckFailed
                        {
                            if let Some(item) = &reason.item {
                                let lock_entry: LockEntry =
                                    serde_dynamo::from_item(item.clone())
                                        .internal("failed to deserialize lock entry")?;
                                let resource_description = lock_entry.description;
                                let repository = lock_entry.repository;

                                // This path executes when a lock exists but is owned by other user
                                warn!(
                                    "Could not unlock {resource_description} in repository {repository}, lock owned by another user (expected {owner_id})"
                                );
                                return Err(LockNotOwned.into());
                            } else {
                                // This path executes when there is no entry in the DB that matches
                                warn!("Reason does not contain a valid item.");
                            }
                        } else if batch_statement_error
                            == BatchStatementErrorCodeEnum::ThrottlingError
                        {
                            return Err(SlowDown.into());
                        } else {
                            if let Some(item) = &reason.item {
                                let lock_entry: LockEntry =
                                    serde_dynamo::from_item(item.clone())
                                        .internal("failed to deserialize lock entry")?;
                                warn!(
                                    "Fail to unlock resource {} on repository {}",
                                    lock_entry.description, lock_entry.repository
                                );
                            }

                            warn!("Unexpected reason code: {reason_code}");
                            return Err(LockError::internal(
                                "unexpected DynamoDB error code while unlocking",
                            ));
                        }
                    }
                    Err(LockNotFound.into())
                } else if error.code() == Some("ThrottlingException") {
                    // It seems that when DynamoDb is throttling TransactWriteItems calls, if the
                    // entire transaction call is throttled (rather than individual parts of the
                    // transaction) we get back an `TransactWriteItemsError::Unhandled` with a code
                    // of `"ThrottlingException"`, rather than a `TransactionCanceledException` with
                    // a reason of `BatchStatementErrorCodeEnum::ThrottlingError`.
                    warn!(
                        "DynamoDB rate limit exceeded while unlocking {} resources for {owner_id} in repository {repository} {error:?}",
                        resources.len()
                    );
                    Err(SlowDown.into())
                } else if let Some(e) = error.as_service_error() {
                    warn!(
                        e = ?e,
                        owner_id,
                        "Unexpected transact write items error while unlocking resources",
                    );
                    Err(LockError::internal(
                        "unexpected transact write items error while unlocking",
                    ))
                } else {
                    warn!("Cannot treat error as service_error: {error}");
                    Err(LockError::internal("cannot treat error as service error"))
                }
            }
            Err(error) => {
                warn!("Unexpected error: {error}");
                Err(LockError::internal(
                    "unexpected error while unlocking resources",
                ))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::client::result::ServiceError;
    use aws_smithy_types::Blob;
    use aws_smithy_types::body::SdkBody;
    use bytes::Bytes;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use mockall::predicate::eq;
    use serde::Deserialize;
    use serde::Serialize;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::dynamodb::MockDynamoDb;
    use crate::dynamodb::error::SdkError;
    use crate::dynamodb::operation::query::QueryError;
    use crate::dynamodb::operation::query::QueryOutput;
    use crate::dynamodb::operation::transact_write_items::TransactWriteItemsOutput;
    use crate::dynamodb::types::CancellationReason;
    use crate::dynamodb::types::error::ResourceNotFoundException;
    use crate::dynamodb::types::error::TransactionCanceledException;

    const TABLE_NAME: &str = "locks-test";

    #[tokio::test]
    async fn test_lock_resources() {
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123";
        let repository = RepositoryId::default();

        let mut dynamodb_mock = MockDynamoDb::default();
        let hash_clone = hash;

        dynamodb_mock
            .expect_transact_write_items()
            .withf(move |items: &Vec<TransactWriteItem>| {
                if items.len() != 1 {
                    println!("Wrong item count: {}", items.len());
                    return false;
                }

                let Some(put_item) = &items[0].put else {
                    println!("Missing put object");
                    return false;
                };

                let Some(item_hash) = put_item.item.get(HASH_KEY) else {
                    println!("Missing hash key");
                    return false;
                };

                let Ok(item_hash) = item_hash.as_b() else {
                    println!("Failed to convert hash to binary blob");
                    return false;
                };

                put_item.table_name == TABLE_NAME && item_hash.as_ref() == hash_clone.as_bytes()
            })
            .return_once(move |_| Ok(TransactWriteItemsOutput::builder().build()));

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let resources = vec![LockResource {
            branch,
            hash,
            description: description.clone(),
        }];

        let locks = lock_store
            .lock_resources(owner_id, repository, &resources)
            .await
            .expect("LockData resources should not have failed");

        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].owner, owner_id);

        let resource = &locks[0].resource;

        assert_eq!(resource.branch, branch);
        assert_eq!(resource.hash, hash);
        assert_eq!(resource.description, description);
    }

    #[tokio::test]
    async fn test_lock_exists() {
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123";
        let repository = RepositoryId::default();

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_transact_write_items()
            .return_once(move |_| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(TransactWriteItemsError::TransactionCanceledException(
                            TransactionCanceledException::builder()
                                .message("exists")
                                .cancellation_reasons(
                                    CancellationReason::builder()
                                        .code("ConditionalCheckFailed")
                                        .item(
                                            "description",
                                            AttributeValue::S("useless".to_string()),
                                        )
                                        .item("branch", AttributeValue::B(branch.as_bytes().into()))
                                        .item("hash", AttributeValue::B(hash.as_bytes().into()))
                                        .item("ownerId", AttributeValue::S("test234".to_string()))
                                        .item(
                                            "repository",
                                            AttributeValue::B(repository.as_bytes().into()),
                                        )
                                        .item(
                                            "repositoryBranch",
                                            AttributeValue::B(
                                                [repository.data(), branch.as_bytes()]
                                                    .concat()
                                                    .as_bytes()
                                                    .into(),
                                            ),
                                        )
                                        .item(
                                            "timestamp",
                                            AttributeValue::S(
                                                "2025-01-23T08:50:34.491599+00:00".to_string(),
                                            ),
                                        )
                                        .build(),
                                )
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let resources = vec![LockResource {
            branch,
            hash,
            description: description.clone(),
        }];

        let err = lock_store
            .lock_resources(owner_id, repository, &resources)
            .await
            .expect_err("LockData resources should have failed conditional check");

        assert!(matches!(err, LockError::LockNotOwned(_)));
    }

    #[tokio::test]
    async fn test_storage_failure() {
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123";
        let repository = RepositoryId::default();

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_transact_write_items()
            .return_once(move |_| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(TransactWriteItemsError::TransactionCanceledException(
                            TransactionCanceledException::builder()
                                .message("error")
                                .cancellation_reasons(
                                    CancellationReason::builder()
                                        .code("ItemCollectionSizeLimitExceeded")
                                        .build(),
                                )
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let resources = vec![LockResource {
            branch,
            hash,
            description: description.clone(),
        }];

        let err = lock_store
            .lock_resources(owner_id, repository, &resources)
            .await
            .expect_err("LockData resources should have thrown a service error");

        assert!(matches!(err, LockError::Internal(_)));
    }

    #[tokio::test]
    async fn test_unlock_resources() {
        let user_id = "localuser";
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let repository = RepositoryId::default();

        let mut dynamodb_mock = MockDynamoDb::default();
        let hash_clone = hash;

        dynamodb_mock
            .expect_transact_write_items()
            .withf(move |items: &Vec<TransactWriteItem>| {
                if items.len() != 1 {
                    println!("Wrong item count: {}", items.len());
                    return false;
                }

                let Some(delete_item) = &items[0].delete else {
                    println!("Missing delete object");
                    return false;
                };

                let Some(item_hash) = delete_item.key.get(HASH_KEY) else {
                    println!("Missing hash key");
                    return false;
                };

                let Ok(item_hash) = item_hash.as_b() else {
                    println!("Failed to convert hash to binary blob");
                    return false;
                };

                delete_item.table_name == TABLE_NAME && item_hash.as_ref() == hash_clone.as_bytes()
            })
            .return_once(move |_| Ok(TransactWriteItemsOutput::builder().build()));

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let resources = vec![LockResource {
            branch,
            hash,
            description: description.clone(),
        }];

        lock_store
            .unlock_resources(user_id, true, repository, &resources)
            .await
            .expect("Unlock resources should not have failed");
    }

    #[tokio::test]
    async fn test_unlock_resources_not_locked() {
        let user_id = "localuser";
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let repository = RepositoryId::default();

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_transact_write_items()
            .return_once(move |_| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(TransactWriteItemsError::TransactionCanceledException(
                            TransactionCanceledException::builder()
                                .message("does not exist")
                                .cancellation_reasons(
                                    CancellationReason::builder()
                                        .code("ConditionalCheckFailed")
                                        .build(),
                                )
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let resources = vec![LockResource {
            branch,
            hash,
            description: description.clone(),
        }];

        lock_store
            .unlock_resources(user_id, true, repository, &resources)
            .await
            .expect_err("Unlock resources should have failed");
    }

    #[tokio::test]
    async fn test_hash_query() {
        let foo_branch: BranchId = rand::random();
        let bar_branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123".to_string();
        let repository = RepositoryId::default();

        let lock_entry_foo = LockEntry {
            description: description.clone(),
            branch: foo_branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(foo_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let lock_entry_bar = LockEntry {
            description: description.clone(),
            branch: bar_branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(bar_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::Hash(hash);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry_foo).unwrap())
                    .items(serde_dynamo::to_item(lock_entry_bar).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Hash query should have succeeded");

        assert_eq!(locks.len(), 2);

        assert_eq!(locks[0].owner, owner_id);
        assert_eq!(locks[1].owner, owner_id);

        let foo_resource = &locks[0].resource;
        let bar_resource = &locks[1].resource;

        assert_eq!(foo_resource.branch, foo_branch);
        assert_eq!(bar_resource.branch, bar_branch);

        assert_eq!(foo_resource.hash, hash);
        assert_eq!(bar_resource.hash, hash);

        assert_eq!(foo_resource.description, description);
        assert_eq!(bar_resource.description, description);
    }

    #[tokio::test]
    async fn test_hash_repo_query() {
        let foo_branch: BranchId = rand::random();
        let bar_branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123".to_string();
        let repository = RepositoryId::default();

        let lock_entry_foo = LockEntry {
            description: description.clone(),
            branch: foo_branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(foo_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let lock_entry_bar = LockEntry {
            description: description.clone(),
            branch: bar_branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(bar_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::HashRepository(hash, repository);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry_foo).unwrap())
                    .items(serde_dynamo::to_item(lock_entry_bar).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Hash repo query should have succeeded");

        assert_eq!(locks.len(), 2);

        assert_eq!(locks[0].owner, owner_id);
        assert_eq!(locks[1].owner, owner_id);

        let foo_resource = &locks[0].resource;
        let bar_resource = &locks[1].resource;

        assert_eq!(foo_resource.branch, foo_branch);
        assert_eq!(bar_resource.branch, bar_branch);

        assert_eq!(foo_resource.hash, hash);
        assert_eq!(bar_resource.hash, hash);

        assert_eq!(foo_resource.description, description);
        assert_eq!(bar_resource.description, description);
    }

    #[tokio::test]
    async fn test_hash_repo_branch_query() {
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123".to_string();
        let repository = RepositoryId::default();
        let repo_branch: Vec<u8> = repository
            .data()
            .iter()
            .chain(branch.data().iter())
            .copied()
            .collect();

        let lock_entry = LockEntry {
            description: description.clone(),
            branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: Bytes::copy_from_slice(repo_branch.as_slice()),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::HashRepositoryBranch(hash, repository, branch);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Hash repo branch query should have succeeded");

        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].owner, owner_id);

        let resource = &locks[0].resource;

        assert_eq!(resource.branch, branch);
        assert_eq!(resource.hash, hash);
        assert_eq!(resource.description, description);
    }

    #[tokio::test]
    async fn test_owner_query() {
        let owner_id = "test123".to_string();

        let foo_branch: BranchId = rand::random();
        let foo_description = "/my/test/file.txt".to_string();
        let foo_hash: Hash = rand::random();
        let foo_repo: RepositoryId = rand::random();

        let lock_entry_foo = LockEntry {
            description: foo_description.clone(),
            branch: foo_branch,
            hash: foo_hash,
            owner_id: owner_id.clone(),
            repository: foo_repo,
            repository_branch: foo_repo
                .data()
                .iter()
                .chain(foo_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let bar_branch: BranchId = rand::random();
        let bar_description = "/my/test2/file2.txt".to_string();
        let bar_hash: Hash = rand::random();
        let bar_repo: RepositoryId = rand::random();

        let lock_entry_bar = LockEntry {
            description: bar_description.clone(),
            branch: bar_branch,
            hash: bar_hash,
            owner_id: owner_id.clone(),
            repository: bar_repo,
            repository_branch: bar_repo
                .data()
                .iter()
                .chain(bar_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::Owner(owner_id.clone());

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry_foo).unwrap())
                    .items(serde_dynamo::to_item(lock_entry_bar).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Owner query should have succeeded");

        assert_eq!(locks.len(), 2);

        assert_eq!(locks[0].owner, owner_id);
        assert_eq!(locks[1].owner, owner_id);

        let foo_resource = &locks[0].resource;
        let bar_resource = &locks[1].resource;

        assert_eq!(foo_resource.branch, foo_branch);
        assert_eq!(bar_resource.branch, bar_branch);

        assert_eq!(foo_resource.hash, foo_hash);
        assert_eq!(bar_resource.hash, bar_hash);

        assert_eq!(foo_resource.description, foo_description);
        assert_eq!(bar_resource.description, bar_description);
    }

    #[tokio::test]
    async fn test_owner_repo_query() {
        let owner_id = "test123".to_string();
        let repository = RepositoryId::default();

        let foo_branch: BranchId = rand::random();
        let foo_description = "/my/test/file.txt".to_string();
        let foo_hash: Hash = rand::random();

        let lock_entry_foo = LockEntry {
            description: foo_description.clone(),
            branch: foo_branch,
            hash: foo_hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(foo_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let bar_branch: BranchId = rand::random();
        let bar_description = "/my/test2/file2.txt".to_string();
        let bar_hash: Hash = rand::random();

        let lock_entry_bar = LockEntry {
            description: bar_description.clone(),
            branch: bar_branch,
            hash: bar_hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(bar_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::OwnerRepository(owner_id.clone(), repository);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry_foo).unwrap())
                    .items(serde_dynamo::to_item(lock_entry_bar).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Owner repo query should have succeeded");

        assert_eq!(locks.len(), 2);

        assert_eq!(locks[0].owner, owner_id);
        assert_eq!(locks[1].owner, owner_id);

        let foo_resource = &locks[0].resource;
        let bar_resource = &locks[1].resource;

        assert_eq!(foo_resource.branch, foo_branch);
        assert_eq!(bar_resource.branch, bar_branch);

        assert_eq!(foo_resource.hash, foo_hash);
        assert_eq!(bar_resource.hash, bar_hash);

        assert_eq!(foo_resource.description, foo_description);
        assert_eq!(bar_resource.description, bar_description);
    }

    #[tokio::test]
    async fn test_owner_repo_branch_query() {
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123".to_string();
        let repository = RepositoryId::default();
        let repo_branch: Vec<u8> = repository
            .data()
            .iter()
            .chain(branch.data().iter())
            .copied()
            .collect();

        let lock_entry = LockEntry {
            description: description.clone(),
            branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: Bytes::from_owner(repo_branch),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::OwnerRepositoryBranch(owner_id.clone(), repository, branch);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Owner repo branch query should have succeeded");

        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].owner, owner_id);

        let resource = &locks[0].resource;

        assert_eq!(resource.branch, branch);
        assert_eq!(resource.hash, hash);
        assert_eq!(resource.description, description);
    }

    #[tokio::test]
    async fn test_repo_query() {
        let repository = RepositoryId::default();

        let foo_branch: BranchId = rand::random();
        let foo_description = "/my/test/file.txt".to_string();
        let foo_hash: Hash = rand::random();
        let foo_owner_id = "test123".to_string();

        let lock_entry_foo = LockEntry {
            description: foo_description.clone(),
            branch: foo_branch,
            hash: foo_hash,
            owner_id: foo_owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(foo_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let bar_branch: BranchId = rand::random();
        let bar_description = "/my/test2/file2.txt".to_string();
        let bar_hash: Hash = rand::random();
        let bar_owner_id = "test456".to_string();

        let lock_entry_bar = LockEntry {
            description: bar_description.clone(),
            branch: bar_branch,
            hash: bar_hash,
            owner_id: bar_owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(bar_branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::Repository(repository);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry_foo).unwrap())
                    .items(serde_dynamo::to_item(lock_entry_bar).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Repo query should have succeeded");

        assert_eq!(locks.len(), 2);

        assert_eq!(locks[0].owner, foo_owner_id);
        assert_eq!(locks[1].owner, bar_owner_id);

        let foo_resource = &locks[0].resource;
        let bar_resource = &locks[1].resource;

        assert_eq!(foo_resource.branch, foo_branch);
        assert_eq!(bar_resource.branch, bar_branch);

        assert_eq!(foo_resource.hash, foo_hash);
        assert_eq!(bar_resource.hash, bar_hash);

        assert_eq!(foo_resource.description, foo_description);
        assert_eq!(bar_resource.description, bar_description);
    }

    #[tokio::test]
    async fn test_repo_branch_query() {
        let repository = RepositoryId::default();
        let branch: BranchId = rand::random();

        let foo_description = "/my/test/file.txt".to_string();
        let foo_hash: Hash = rand::random();
        let foo_owner_id = "test123".to_string();

        let lock_entry_foo = LockEntry {
            description: foo_description.clone(),
            branch,
            hash: foo_hash,
            owner_id: foo_owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let bar_description = "/my/test2/file2.txt".to_string();
        let bar_hash: Hash = rand::random();
        let bar_owner_id = "test456".to_string();

        let lock_entry_bar = LockEntry {
            description: bar_description.clone(),
            branch,
            hash: bar_hash,
            owner_id: bar_owner_id.clone(),
            repository,
            repository_branch: repository
                .data()
                .iter()
                .chain(branch.data().iter())
                .copied()
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::RepositoryBranch(repository, branch);

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry_foo).unwrap())
                    .items(serde_dynamo::to_item(lock_entry_bar).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Repo branch query should have succeeded");

        assert_eq!(locks.len(), 2);

        assert_eq!(locks[0].owner, foo_owner_id);
        assert_eq!(locks[1].owner, bar_owner_id);

        let foo_resource = &locks[0].resource;
        let bar_resource = &locks[1].resource;

        assert_eq!(foo_resource.branch, branch);
        assert_eq!(bar_resource.branch, branch);

        assert_eq!(foo_resource.hash, foo_hash);
        assert_eq!(bar_resource.hash, bar_hash);

        assert_eq!(foo_resource.description, foo_description);
        assert_eq!(bar_resource.description, bar_description);
    }

    #[tokio::test]
    async fn test_repo_branch_desc_query() {
        let repository = RepositoryId::default();
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123".to_string();
        let repo_branch: Vec<u8> = repository
            .data()
            .iter()
            .chain(branch.data().iter())
            .copied()
            .collect();

        let lock_entry = LockEntry {
            description: description.clone(),
            branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: Bytes::from_owner(repo_branch),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let query = LockQuery::RepositoryBranchDescription(repository, branch, description.clone());

        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
            .return_once(move |_, _| {
                Ok(QueryOutput::builder()
                    .items(serde_dynamo::to_item(lock_entry).unwrap())
                    .build()
                    .into())
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let locks = lock_store
            .query_locks(query)
            .await
            .expect("Repo branch description query should have succeeded");

        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].owner, owner_id);

        let resource = &locks[0].resource;

        assert_eq!(resource.branch, branch);
        assert_eq!(resource.hash, hash);
        assert_eq!(resource.description, description);
    }

    #[tokio::test]
    async fn test_query_failure() {
        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_query_paginated()
            .return_once(move |_, _: LockQuery| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(QueryError::ResourceNotFoundException(
                            ResourceNotFoundException::builder()
                                .message(format!("Table missing: {TABLE_NAME}"))
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            });

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let hash: Hash = rand::random();
        let query = LockQuery::Hash(hash);

        let err = lock_store
            .query_locks(query)
            .await
            .expect_err("Query should have thrown a service error");

        assert!(matches!(err, LockError::Internal(_)));
    }

    #[tokio::test]
    async fn unlock_fails_for_other_owner() {
        let mut dynamodb_mock = MockDynamoDb::default();

        let repository = RepositoryId::default();
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();
        let owner_id = "test123".to_string();
        let repo_branch: Vec<u8> = repository
            .data()
            .iter()
            .chain(branch.data().iter())
            .copied()
            .collect();

        let lock_entry = LockEntry {
            description: description.clone(),
            branch,
            hash,
            owner_id: owner_id.clone(),
            repository,
            repository_branch: Bytes::from_owner(repo_branch),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        dynamodb_mock.expect_transact_write_items().return_once(
            move |_: Vec<TransactWriteItem>| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(TransactWriteItemsError::TransactionCanceledException(
                            TransactionCanceledException::builder()
                                .cancellation_reasons(
                                    CancellationReason::builder()
                                        .code(
                                            BatchStatementErrorCodeEnum::ConditionalCheckFailed
                                                .as_str(),
                                        )
                                        .set_item(Some(serde_dynamo::to_item(lock_entry).unwrap()))
                                        .build(),
                                )
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            },
        );

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let err = lock_store
            .unlock_resources(
                "some-owner",
                true,
                repository,
                &[LockResource {
                    branch,
                    hash,
                    description,
                }],
            )
            .await
            .expect_err("Unlock should have returned an Error");

        assert!(matches!(err, LockError::LockNotOwned(_)));
    }

    #[tokio::test]
    async fn unlock_returns_storage_error_for_unhandled_service_error() {
        let mut dynamodb_mock = MockDynamoDb::default();

        let repository = RepositoryId::default();
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();

        dynamodb_mock.expect_transact_write_items().return_once(
            move |_: Vec<TransactWriteItem>| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(TransactWriteItemsError::ResourceNotFoundException(
                            ResourceNotFoundException::builder()
                                .message("Oh dear")
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            },
        );

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let err = lock_store
            .unlock_resources(
                "some-owner",
                true,
                repository,
                &[LockResource {
                    branch,
                    hash,
                    description,
                }],
            )
            .await
            .expect_err("Unlock should have returned an Error");

        assert!(matches!(err, LockError::Internal(_)));
    }

    #[tokio::test]
    async fn unlock_returns_slow_down_for_batch_statement_error_code() {
        let mut dynamodb_mock = MockDynamoDb::default();

        let repository = RepositoryId::default();
        let branch: BranchId = rand::random();
        let description = "/my/test/file.txt".to_string();
        let hash: Hash = rand::random();

        dynamodb_mock.expect_transact_write_items().return_once(
            move |_: Vec<TransactWriteItem>| {
                Err(AwsError::AwsSdkError(SdkError::ServiceError(
                    ServiceError::builder()
                        .source(TransactWriteItemsError::TransactionCanceledException(
                            TransactionCanceledException::builder()
                                .cancellation_reasons(
                                    CancellationReason::builder()
                                        .code("ThrottlingError")
                                        .build(),
                                )
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            400u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            },
        );

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let err = lock_store
            .unlock_resources(
                "some-owner",
                true,
                repository,
                &[LockResource {
                    branch,
                    hash,
                    description,
                }],
            )
            .await
            .expect_err("Unlock should have returned an Error");

        assert!(matches!(err, LockError::SlowDown(_)));
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    pub struct Foo {
        address: Address,
        context: Context,
        hash: lore_storage::Hash,
    }

    impl Default for Foo {
        fn default() -> Self {
            Foo {
                address: Address {
                    hash: lore_storage::Hash::from([5; 32]),
                    context: Context::from([6; 16]),
                },
                hash: lore_storage::Hash::from([7; 32]),
                context: Context::from([8; 16]),
            }
        }
    }

    #[test]
    fn test_dynamo_serde_with_addresses() {
        // from blob attributes for string and context
        let attributes = HashMap::from([
            ("address".to_string(), AttributeValue::S("0505050505050505050505050505050505050505050505050505050505050505-06060606060606060606060606060606".to_string())),
            ("hash".to_string(), AttributeValue::B(Blob::new([7; 32]))),
            ("context".to_string(), AttributeValue::B(Blob::new([8; 16])))
        ]);
        let default_item = Foo::default();
        let from_item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(&default_item).unwrap();
        assert_eq!(from_item, attributes);

        let from_attributes: Foo = serde_dynamo::from_item(attributes).unwrap();
        assert_eq!(default_item, from_attributes);

        // from string attributes for hash and context
        let attributes = HashMap::from([
            ("address".to_string(), AttributeValue::S("0505050505050505050505050505050505050505050505050505050505050505-06060606060606060606060606060606".to_string())),
            ("hash".to_string(), AttributeValue::S("0707070707070707070707070707070707070707070707070707070707070707".to_string())),
            ("context".to_string(), AttributeValue::S("08080808080808080808080808080808".to_string())),
        ]);
        let from_attributes: Foo = serde_dynamo::from_item(attributes).unwrap();
        assert_eq!(default_item, from_attributes);
    }
}
