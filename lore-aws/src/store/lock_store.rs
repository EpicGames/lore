// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::LockNotFound;
use lore_base::error::LockNotOwned;
use lore_base::error::SlowDown;
use lore_base::timestamp;
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
use smallvec::smallvec;
use time::UtcDateTime;
use tracing::debug;
use tracing::info_span;
use tracing::warn;
use uuid::Uuid;
use zerocopy::IntoBytes;

use crate::aws::error::BatchStatementErrorCode;
use crate::aws::error::CancellationReason;
use crate::aws::error::is_overloaded;
use crate::aws::item::AttributeValue;
use crate::aws::item::from_item;
use crate::aws::item::to_item;
use crate::aws_error::AwsError;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbQuery;
use crate::dynamodb::DynamoError;
use crate::dynamodb::ExprNames;
use crate::dynamodb::ExprValues;
use crate::dynamodb::TransactDelete;
use crate::dynamodb::TransactPut;
use crate::dynamodb::TransactWriteItem;
use crate::dynamodb::cancellation_reason::interesting_cancellation_reason_filter;
use crate::store::is_store_overloaded;
use crate::store::with_backoff;

// If there is no code to an AWS error, rather than having nothing (which won't get added to a trace
// event) fallback to something so we can at least aggregate on a common term and also be sure the
// events are working
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

#[lore_macro::test_pub]
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
        let locked_at = timestamp::parse_rfc3339_millis(value.timestamp.as_str())
            .internal("failed to parse timestamp")?;
        let resource = LockResource {
            branch: value.branch,
            hash: value.hash,
            description: value.description,
        };
        Ok(Self {
            resource,
            owner: value.owner_id,
            locked_at: locked_at as u64,
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
    fn index_name(&self) -> Option<&'static str> {
        match self {
            LockQuery::Owner(_)
            | LockQuery::OwnerRepository(_, _)
            | LockQuery::OwnerRepositoryBranch(_, _, _) => Some(OWNER_REPO_BRANCH_GSI),
            LockQuery::Repository(_) | LockQuery::RepositoryBranch(_, _) => Some(REPO_BRANCH_GSI),
            LockQuery::RepositoryBranchDescription(_, _, _) => Some(REPO_BRANCH_DESC_GSI),
            _ => None,
        }
    }

    fn key_condition_expression(&self) -> &str {
        match self {
            LockQuery::Hash(_) => "#pk = :hash",
            LockQuery::HashRepository(_, _) => "#pk = :hash and begins_with(#sk, :repo)",
            LockQuery::HashRepositoryBranch(_, _, _) => "#pk = :hash and #sk = :repoBranch",
            LockQuery::Owner(_) => "#pk = :owner",
            LockQuery::OwnerRepository(_, _) => "#pk = :owner and begins_with(#sk, :repo)",
            LockQuery::OwnerRepositoryBranch(_, _, _) => "#pk = :owner and #sk = :repoBranch",
            LockQuery::Repository(_) => "#pk = :repo",
            LockQuery::RepositoryBranch(_, _) => "#pk = :repo and #sk = :branch",
            LockQuery::RepositoryBranchDescription(_, _, _) => {
                "#pk = :repoBranch and #sk = :description"
            }
        }
    }

    fn expression_attribute_names(&self) -> ExprNames {
        match self {
            LockQuery::Hash(_) => ExprNames::from_slice(&[("#pk", HASH_KEY)]),
            LockQuery::HashRepository(_, _) | LockQuery::HashRepositoryBranch(_, _, _) => {
                ExprNames::from_slice(&[("#pk", HASH_KEY), ("#sk", REPO_BRANCH_KEY)])
            }
            LockQuery::Owner(_) => ExprNames::from_slice(&[("#pk", OWNER_KEY)]),
            LockQuery::OwnerRepository(_, _) | LockQuery::OwnerRepositoryBranch(_, _, _) => {
                ExprNames::from_slice(&[("#pk", OWNER_KEY), ("#sk", REPO_BRANCH_KEY)])
            }
            LockQuery::Repository(_) => ExprNames::from_slice(&[("#pk", REPO_KEY)]),
            LockQuery::RepositoryBranch(_, _) => {
                ExprNames::from_slice(&[("#pk", REPO_KEY), ("#sk", BRANCH_KEY)])
            }
            LockQuery::RepositoryBranchDescription(_, _, _) => {
                ExprNames::from_slice(&[("#pk", REPO_BRANCH_KEY), ("#sk", DESC_KEY)])
            }
        }
    }

    fn expression_attribute_values(&self) -> ExprValues {
        /// The sort key on the repository-branch indexes: the two identifiers, concatenated.
        fn repository_branch(repository: &RepositoryId, branch: &BranchId) -> AttributeValue {
            const HALF: usize = size_of::<lore_base::types::Context>();

            let mut key = [0u8; HALF * 2];
            let (first, second) = key.split_at_mut(HALF);
            first.copy_from_slice(repository.data());
            second.copy_from_slice(branch.data());

            AttributeValue::blob(key)
        }

        match self {
            LockQuery::Hash(hash) => {
                smallvec![(":hash", AttributeValue::blob(hash.as_bytes()))]
            }
            LockQuery::HashRepository(hash, repository) => smallvec![
                (":hash", AttributeValue::blob(hash.as_bytes())),
                (":repo", AttributeValue::blob(repository.as_bytes())),
            ],
            LockQuery::HashRepositoryBranch(hash, repository, branch) => smallvec![
                (":hash", AttributeValue::blob(hash.as_bytes())),
                (":repoBranch", repository_branch(repository, branch)),
            ],
            LockQuery::Owner(owner) => {
                smallvec![(":owner", AttributeValue::string(owner))]
            }
            LockQuery::OwnerRepository(owner, repository) => smallvec![
                (":owner", AttributeValue::string(owner)),
                (":repo", AttributeValue::blob(repository.as_bytes())),
            ],
            LockQuery::OwnerRepositoryBranch(owner, repository, branch) => smallvec![
                (":owner", AttributeValue::string(owner)),
                (":repoBranch", repository_branch(repository, branch)),
            ],
            LockQuery::Repository(repository) => {
                smallvec![(":repo", AttributeValue::blob(repository.as_bytes()))]
            }
            LockQuery::RepositoryBranch(repository, branch) => smallvec![
                (":repo", AttributeValue::blob(repository.as_bytes())),
                (":branch", AttributeValue::blob(branch.as_bytes())),
            ],
            LockQuery::RepositoryBranchDescription(repository, branch, description) => smallvec![
                (":repoBranch", repository_branch(repository, branch)),
                (":description", AttributeValue::string(description)),
            ],
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
    let code = reason.code().map(BatchStatementErrorCode::from);

    if code
        .as_ref()
        .is_some_and(BatchStatementErrorCode::is_transient)
    {
        debug!(
            code = ?code,
            message = ?reason.message,
            "DynamoDB declined a lock entry for load, backing off"
        );
        return Err(SlowDown.into());
    }

    code.filter(|error_code| {
        matches!(
            error_code,
            BatchStatementErrorCode::ConditionalCheckFailed
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
            from_item((**item).clone()).internal("failed to deserialize lock entry")?;

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

impl DynamoDbLockStore {
    /// One attempt at locking `resources`.
    ///
    /// Each transaction goes out under a fresh request token: its items carry this attempt's
    /// timestamp and shrink between rounds, so no two are the same request, and a replay of one
    /// that applied unseen comes back as a lock the caller already holds, which is dropped from the
    /// request rather than reported.
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::lock_resources", skip_all)]
    async fn lock_resources_once(
        &self,
        owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let mut locks = HashMap::<LockEntryKey, LockData>::with_capacity(resources.len());
        let mut write_items =
            HashMap::<LockEntryKey, TransactWriteItem>::with_capacity(resources.len());

        // Use the same timestamp for all locks
        let now = UtcDateTime::now();

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
                timestamp: timestamp::rfc3339(now),
            };

            let item = to_item(&lock_entry).internal("failed to serialize lock entry")?;

            let put_item = TransactPut::new(self.table_name.clone(), item)
                // Since PutItem will pull up an existing PK + SK combo to begin with,
                // only checking for existence of the PK attr is necessary
                .condition("attribute_not_exists(#pk)")
                .name("#pk", HASH_KEY)
                .returning_old_on_failure();

            write_items.insert(candidate_key.clone(), TransactWriteItem::Put(put_item));

            locks.insert(
                candidate_key.clone(),
                LockData {
                    resource: resource.clone(),
                    owner: owner_id.to_string(),
                    locked_at: timestamp::unix_millis(now) as u64,
                },
            );
        }

        loop {
            let result = self
                .dynamodb
                .transact_write_items(
                    &self.table_name,
                    write_items.values().cloned().collect(),
                    Uuid::new_v4(),
                )
                .await;

            // If the transaction failed because of locks already exist
            // a) check if the owner is the user that issue the lock request, remove it and retry
            //    lock
            // b) otherwise the request needs to fail
            match result {
                Ok(_) => break,
                Err(AwsError::Aws(error)) => {
                    let span = info_span!(
                        "transact_write_items_error",
                        aws_error_code = error.code().unwrap_or(TRACING_AWS_NO_CODE_FALLBACK),
                        resources_length = resources.len()
                    );
                    let _ = span.or_current().enter();

                    match error.as_service_error() {
                        Some(service) if service.code.is_transaction_canceled_exception() => {
                            let outstanding = write_items.len();

                            for reason in service
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

                            // Going round again is worth it only because the set shrank, which
                            // bounds this at one round per resource. A cancellation that names
                            // nothing this request holds — no reasons at all, every reason coded
                            // `None`, or a row that maps to no requested resource — leaves the
                            // identical transaction to send, and sending it again is a hot loop
                            // against a service that has already refused it.
                            if write_items.len() == outstanding {
                                warn!(
                                    owner_id,
                                    %repository,
                                    resources = resources.len(),
                                    reasons = service.cancellation_reasons().len(),
                                    "DynamoDB cancelled a lock acquisition without naming a lock this \
                                     request holds; not sending it again"
                                );

                                return Err(LockError::internal(
                                    "lock transaction cancelled without a reason naming a requested \
                                     resource",
                                ));
                            }
                        }
                        // A transaction shed as a whole is a back-off for the ladder above.
                        _ if is_overloaded(&error) => {
                            debug!(
                                owner_id,
                                %repository,
                                resources = resources.len(),
                                error = ?error,
                                "A lock acquisition was shed, backing off"
                            );
                            return Err(SlowDown.into());
                        }
                        Some(service) => {
                            warn!(
                                owner_id,
                                %repository,
                                resources = resources.len(),
                                error = ?service,
                                "Unexpected transact write items error while locking"
                            );
                            return Err(LockError::internal(
                                "unexpected transact write items error while locking",
                            ));
                        }
                        None => {
                            warn!(error = ?error, "Cannot treat error as service_error");
                            return Err(LockError::internal("cannot treat error as service error"));
                        }
                    }
                }
                Err(error) => {
                    warn!(error = ?error, "Unexpected error while locking resources");
                    return Err(LockError::internal(
                        "unexpected error while locking resources",
                    ));
                }
            }
        }

        Ok(locks.into_values().collect())
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::query_locks", skip_all)]
    async fn query_locks_once(&self, query: LockQuery) -> Result<Vec<LockData>, LockError> {
        let output = self
            .dynamodb
            .query_paginated(&self.table_name, query)
            .await
            .map_err(shedding_or_internal("failed to query locks"))?;

        let mut locks = vec![];

        if let Some(items) = output.items {
            for item in items {
                let lock_entry: LockEntry =
                    from_item(item).internal("failed to deserialize lock entry")?;

                locks.push(LockData::try_from(lock_entry)?);
            }
        }

        Ok(locks)
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::check_locks_status", skip_all)]
    async fn check_locks_status_once(
        &self,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let mut items = Vec::with_capacity(resources.len());

        for resource in resources {
            let lock_key = LockKey {
                hash: resource.hash,
                repository_branch: Bytes::from_owner(
                    [*repository.data(), *resource.branch.data()].concat(),
                ),
            };

            items.push(to_item(&lock_key).internal("failed to serialize lock key")?);
        }

        let results = self
            .dynamodb
            .batch_get_item(&self.table_name, items, false /* consistent */)
            .await
            .map_err(shedding_or_internal("failed to batch get lock items"))?;

        let mut output = Vec::with_capacity(results.len());

        for item in results {
            let lock: LockEntry = from_item(item).internal("failed to deserialize lock entry")?;

            output.push(lock.try_into()?);
        }

        Ok(output)
    }

    /// One attempt at unlocking `resources`, under the request token `token`.
    ///
    /// Every attempt of one unlock sends the same items under the same `token`, so an attempt after
    /// one that applied unseen is answered with that success instead of finding its locks gone.
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "DynamoDbLockStore::unlock_resources", skip_all)]
    async fn unlock_resources_once(
        &self,
        owner_id: &str,
        validate_user: bool,
        repository: RepositoryId,
        resources: &[LockResource],
        token: Uuid,
    ) -> Result<Vec<LockResource>, LockError> {
        let mut write_items = Vec::with_capacity(resources.len());

        for resource in resources {
            let lock_key = LockKey {
                hash: resource.hash,
                repository_branch: Bytes::from_owner(
                    [*repository.data(), *resource.branch.data()].concat(),
                ),
            };

            let key = to_item(&lock_key).internal("failed to serialize lock key")?;

            let delete_item = if validate_user {
                TransactDelete::new(self.table_name.clone(), key)
                    .condition("ownerId = :val")
                    .value(":val", AttributeValue::string(owner_id))
                    .returning_old_on_failure()
            } else {
                TransactDelete::new(self.table_name.clone(), key)
            };

            write_items.push(TransactWriteItem::Delete(delete_item));
        }

        match self
            .dynamodb
            .transact_write_items(&self.table_name, write_items, token)
            .await
        {
            Ok(_) => Ok(resources.to_vec()),
            Err(AwsError::Aws(error)) => {
                let span = info_span!(
                    "transact_write_items_error",
                    aws_error_code = error.code().unwrap_or(TRACING_AWS_NO_CODE_FALLBACK),
                    resources_length = resources.len()
                );
                let _ = span.or_current().enter();

                if let Some(service) = error
                    .as_service_error()
                    .filter(|service| service.code.is_transaction_canceled_exception())
                {
                    let mut nothing_to_release = false;

                    for reason in service
                        .cancellation_reasons()
                        .iter()
                        .filter(interesting_cancellation_reason_filter)
                    {
                        let reason_code = reason.code().unwrap_or_default();
                        let batch_statement_error = BatchStatementErrorCode::from(reason_code);
                        if batch_statement_error == BatchStatementErrorCode::ConditionalCheckFailed
                        {
                            if let Some(item) = &reason.item {
                                let lock_entry: LockEntry = from_item((**item).clone())
                                    .internal("failed to deserialize lock entry")?;
                                let resource_description = lock_entry.description;
                                let repository = lock_entry.repository;

                                // This path executes when a lock exists but is owned by other user
                                warn!(
                                    "Could not unlock {resource_description} in repository {repository}, lock owned by another user (expected {owner_id})"
                                );
                                return Err(LockNotOwned.into());
                            }

                            // The check failed with no row behind it, so there was no lock to
                            // release. This is the one reason that establishes not-found; the
                            // fall-through below must not claim it without one.
                            nothing_to_release = true;
                        } else if batch_statement_error.is_transient() {
                            debug!(
                                code = reason_code,
                                "DynamoDB declined a lock release entry for load, backing off"
                            );
                            return Err(SlowDown.into());
                        } else {
                            if let Some(item) = &reason.item {
                                let lock_entry: LockEntry = from_item((**item).clone())
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

                    if nothing_to_release {
                        return Err(LockNotFound.into());
                    }

                    // No reason named a lock this request holds, so the release was not applied;
                    // that establishes nothing about whether the locks exist.
                    warn!(
                        owner_id,
                        %repository,
                        resources = resources.len(),
                        reasons = service.cancellation_reasons().len(),
                        "DynamoDB cancelled a lock release without naming a lock this request holds"
                    );

                    Err(LockError::internal(
                        "unlock transaction cancelled without a reason naming a requested resource",
                    ))
                } else if is_overloaded(&error) {
                    debug!(
                        owner_id,
                        %repository,
                        resources = resources.len(),
                        error = ?error,
                        "A lock release was shed, backing off"
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

/// Report a shedding table as a back-off, and anything else as terminal.
///
/// A throttle means the question went unanswered rather than answered badly, so it belongs to
/// [`with_backoff`]'s ladder — which only reconsiders a `SlowDown`. Flattening one to internal
/// makes a busy table indistinguishable from a broken one and skips the wait entirely.
///
/// Shedding is ordinary under load and is already counted: every call labels its latency histogram
/// with the service's own error code, so the rate is a metric rather than something to find by
/// counting log lines. Warning per shed request would bury the failures worth reading.
fn shedding_or_internal(context: &'static str) -> impl Fn(DynamoError) -> LockError {
    move |error| {
        if is_store_overloaded(&error) {
            debug!(context, error = ?error, "A lock store read was shed, backing off");
            SlowDown.into()
        } else {
            warn!(context, error = ?error, "Lock store read failed");
            LockError::internal_with_context(error, context)
        }
    }
}

#[async_trait]
impl LockStore for DynamoDbLockStore {
    async fn lock_resources(
        &self,
        owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        with_backoff(LockError::is_slow_down, || {
            self.lock_resources_once(owner_id, repository, resources)
        })
        .await
    }

    async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError> {
        with_backoff(LockError::is_slow_down, || {
            self.query_locks_once(query.clone())
        })
        .await
    }

    async fn check_locks_status(
        &self,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        with_backoff(LockError::is_slow_down, || {
            self.check_locks_status_once(repository, resources)
        })
        .await
    }

    async fn unlock_resources(
        &self,
        owner_id: &str,
        validate_user: bool,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        let token = Uuid::new_v4();
        with_backoff(LockError::is_slow_down, || {
            self.unlock_resources_once(owner_id, validate_user, repository, resources, token)
        })
        .await
    }

    /// One `DynamoDB` transaction's worth.
    ///
    /// Locking and unlocking are `TransactWriteItems` calls, which the service applies atomically
    /// up to this many items and refuses beyond it. A larger request would be sent as several
    /// transactions, and then a partly-shed request would leave some resources locked and others
    /// not, with the caller told only that the whole thing failed.
    fn max_lock_batch(&self) -> Option<usize> {
        Some(crate::dynamodb::TRANSACT_WRITE_ITEMS_CHUNKS)
    }
}
