// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_aws::aws::dynamodb::QueryOutput;
use lore_aws::aws::dynamodb::WriteOutput;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::CancellationReason;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::error::ErrorPayload;
use lore_aws::aws::error::ServiceError;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::item::from_item;
use lore_aws::aws::item::to_item;
use lore_aws::aws_error::AwsError;
use lore_aws::dynamodb::DynamoError;
use lore_aws::dynamodb::MockDynamoDb;
use lore_aws::dynamodb::TransactWriteItem;
use lore_aws::store::lock_store::*;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::timestamp;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::LockData;
use lore_base::types::LockResource;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use mockall::predicate::eq;
use serde::Deserialize;
use serde::Serialize;
use time::UtcDateTime;
use uuid::Uuid;
use zerocopy::IntoBytes;

const TABLE_NAME: &str = "locks-test";

/// A failure carrying a service code and nothing else.
fn service_failure(code: DynamoErrorCode) -> DynamoError {
    AwsError::Aws(AwsCallError::service(ServiceError {
        status: http::StatusCode::BAD_REQUEST,
        code,
        message: None,
        request_id: None,
        payload: ErrorPayload::None,
    }))
}

/// A cancelled transaction, with one reason per entry given.
fn transaction_canceled(reasons: Vec<CancellationReason>) -> DynamoError {
    AwsError::Aws(AwsCallError::service(ServiceError {
        status: http::StatusCode::BAD_REQUEST,
        code: DynamoErrorCode::TransactionCanceledException,
        message: Some(ByteStr::from_static("exists")),
        request_id: None,
        payload: ErrorPayload::CancellationReasons(reasons),
    }))
}

/// A reason naming `code`, carrying the lock row a conditional check lost against.
fn reason_with_lock(
    code: &'static str,
    owner_id: &str,
    repository: RepositoryId,
    branch: BranchId,
    hash: Hash,
    description: &str,
) -> CancellationReason {
    let entry = LockEntry {
        description: description.to_owned(),
        branch,
        hash,
        owner_id: owner_id.to_owned(),
        repository,
        repository_branch: Bytes::from_owner([*repository.data(), *branch.data()].concat()),
        timestamp: "2025-01-23T08:50:34.491599+00:00".to_owned(),
    };

    CancellationReason {
        code: Some(ByteStr::from_static(code)),
        message: None,
        item: Some(Box::new(to_item(&entry).unwrap())),
    }
}

fn reason(code: &'static str) -> CancellationReason {
    CancellationReason {
        code: Some(ByteStr::from_static(code)),
        message: None,
        item: None,
    }
}

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
        .withf(
            move |_: &Arc<str>, items: &Vec<TransactWriteItem>, _: &Uuid| {
                let [TransactWriteItem::Put(put_item)] = items.as_slice() else {
                    println!("Expected exactly one put, got {items:?}");
                    return false;
                };

                let Some(item_hash) = put_item
                    .item
                    .get(HASH_KEY)
                    .and_then(AttributeValue::as_blob)
                else {
                    println!("Missing or non-binary hash key");
                    return false;
                };

                &*put_item.table_name == TABLE_NAME && item_hash == hash_clone.as_bytes()
            },
        )
        .return_once(move |_, _, _| Ok(vec![WriteOutput::default()]));

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
        .return_once(move |_, _, _| {
            Err(transaction_canceled(vec![reason_with_lock(
                "ConditionalCheckFailed",
                "test234",
                repository,
                branch,
                hash,
                "useless",
            )]))
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
        .return_once(move |_, _, _| {
            Err(transaction_canceled(vec![reason(
                "ItemCollectionSizeLimitExceeded",
            )]))
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
        .withf(
            move |_: &Arc<str>, items: &Vec<TransactWriteItem>, _: &Uuid| {
                let [TransactWriteItem::Delete(delete_item)] = items.as_slice() else {
                    println!("Expected exactly one delete, got {items:?}");
                    return false;
                };

                let Some(item_hash) = delete_item
                    .key
                    .get(HASH_KEY)
                    .and_then(AttributeValue::as_blob)
                else {
                    println!("Missing or non-binary hash key");
                    return false;
                };

                &*delete_item.table_name == TABLE_NAME && item_hash == hash_clone.as_bytes()
            },
        )
        .return_once(move |_, _, _| Ok(vec![WriteOutput::default()]));

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
        .return_once(move |_, _, _| {
            Err(transaction_canceled(vec![reason("ConditionalCheckFailed")]))
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
        .unlock_resources(user_id, true, repository, &resources)
        .await
        .expect_err("Unlock resources should have failed");

    assert!(
        matches!(err, LockError::LockNotFound(_)),
        "a failed check with no row behind it is a missing lock, got {err:?}"
    );
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::Hash(hash);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![
                    to_item(lock_entry_foo).unwrap(),
                    to_item(lock_entry_bar).unwrap(),
                ]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::HashRepository(hash, repository);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![
                    to_item(lock_entry_foo).unwrap(),
                    to_item(lock_entry_bar).unwrap(),
                ]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::HashRepositoryBranch(hash, repository, branch);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![to_item(lock_entry).unwrap()]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::Owner(owner_id.clone());

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![
                    to_item(lock_entry_foo).unwrap(),
                    to_item(lock_entry_bar).unwrap(),
                ]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::OwnerRepository(owner_id.clone(), repository);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![
                    to_item(lock_entry_foo).unwrap(),
                    to_item(lock_entry_bar).unwrap(),
                ]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::OwnerRepositoryBranch(owner_id.clone(), repository, branch);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![to_item(lock_entry).unwrap()]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::Repository(repository);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![
                    to_item(lock_entry_foo).unwrap(),
                    to_item(lock_entry_bar).unwrap(),
                ]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::RepositoryBranch(repository, branch);

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![
                    to_item(lock_entry_foo).unwrap(),
                    to_item(lock_entry_bar).unwrap(),
                ]),
                ..QueryOutput::default()
            }
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    let query = LockQuery::RepositoryBranchDescription(repository, branch, description.clone());

    let mut dynamodb_mock = MockDynamoDb::default();

    dynamodb_mock
        .expect_query_paginated()
        .with(eq(Arc::<str>::from(TABLE_NAME)), eq(query.clone()))
        .return_once(move |_, _| {
            Ok(QueryOutput {
                items: Some(vec![to_item(lock_entry).unwrap()]),
                ..QueryOutput::default()
            }
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
            Err(service_failure(DynamoErrorCode::ResourceNotFoundException))
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
        timestamp: timestamp::rfc3339(UtcDateTime::now()),
    };

    dynamodb_mock.expect_transact_write_items().return_once(
        move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
            Err(transaction_canceled(vec![CancellationReason {
                code: Some(ByteStr::from_static("ConditionalCheckFailed")),
                message: None,
                item: Some(Box::new(to_item(lock_entry).unwrap())),
            }]))
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
        move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
            Err(service_failure(DynamoErrorCode::ResourceNotFoundException))
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

/// Paused time: the ladder is the production one, and spending it for real would put a minute
/// of sleeping in the unit suite.
#[tokio::test(start_paused = true)]
async fn unlock_returns_slow_down_for_batch_statement_error_code() {
    let mut dynamodb_mock = MockDynamoDb::default();

    let repository = RepositoryId::default();
    let branch: BranchId = rand::random();
    let description = "/my/test/file.txt".to_string();
    let hash: Hash = rand::random();

    // `returning` rather than `return_once`: a throttle is waited out at this tier, so the service
    // is asked again until the ladder is spent.
    dynamodb_mock.expect_transact_write_items().returning(
        move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
            Err(transaction_canceled(vec![reason("ThrottlingError")]))
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

/// A resource this test locks or unlocks; the values never matter, only that there is one.
fn one_resource() -> LockResource {
    LockResource {
        branch: rand::random(),
        hash: rand::random(),
        description: "/my/test/file.txt".to_string(),
    }
}

/// The whole transaction declined for throughput, which is not spelled `ThrottlingException`, is a
/// back-off like every other shedding shape.
#[tokio::test(start_paused = true)]
async fn a_shed_lock_acquisition_backs_off() {
    let mut dynamodb_mock = MockDynamoDb::default();
    dynamodb_mock.expect_transact_write_items().returning(
        move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
            Err(service_failure(
                DynamoErrorCode::ProvisionedThroughputExceededException,
            ))
        },
    );

    let lock_store = DynamoDbLockStore {
        dynamodb: dynamodb_mock,
        table_name: Arc::from(TABLE_NAME),
    };

    let err = lock_store
        .lock_resources("some-owner", RepositoryId::default(), &[one_resource()])
        .await
        .expect_err("a shedding table must fail the acquisition");

    assert!(
        matches!(err, LockError::SlowDown(_)),
        "a shed acquisition must be retryable, got {err:?}"
    );
}

/// A cancellation that names nothing is not sent again.
///
/// The loop goes round only because a round shrank the set of items to write. A
/// `TransactionCanceledException` carrying no reasons — which is what the service answers some
/// cancellations with, and what an intermediary that reshapes the body leaves behind — shrinks
/// nothing, so sending it again is a hot loop, with no wait, against a service that has already
/// refused the request. A reason coded `None` says as little and reads the same way.
///
/// Congestion does not arrive here: every shedding shape leaves the loop as a `SlowDown` for the
/// ladder above, which waits between its attempts. This is the terminal case only, so it is
/// reported rather than retried.
#[tokio::test(start_paused = true)]
async fn a_cancellation_naming_nothing_is_not_sent_again() {
    for reasons_are_empty in [true, false] {
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = sent.clone();

        let mut dynamodb_mock = MockDynamoDb::default();
        dynamodb_mock.expect_transact_write_items().returning(
            move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
                let attempt = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(attempt < 4, "the identical transaction was sent again");

                Err(transaction_canceled(if reasons_are_empty {
                    vec![]
                } else {
                    vec![reason("None")]
                }))
            },
        );

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let err = lock_store
            .lock_resources("some-owner", RepositoryId::default(), &[one_resource()])
            .await
            .expect_err("a cancellation that cannot be acted on is not an acquisition");

        assert!(
            matches!(err, LockError::Internal(_)),
            "sending it again cannot help, so this is terminal rather than a back-off: {err:?}"
        );
        assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

/// A cancelled release that names nothing is not a missing lock.
///
/// The reason that establishes not-found is a conditional check that failed with no row behind it.
/// A cancellation carrying no reasons — or only reasons coded `None` — establishes nothing, and
/// answering it with `LockNotFound` would tell a caller its lock had gone when the truth is that
/// its request could not be applied.
#[tokio::test(start_paused = true)]
async fn a_cancelled_release_naming_nothing_is_not_reported_as_missing() {
    for reasons_are_empty in [true, false] {
        let mut dynamodb_mock = MockDynamoDb::default();
        dynamodb_mock.expect_transact_write_items().returning(
            move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
                Err(transaction_canceled(if reasons_are_empty {
                    vec![]
                } else {
                    vec![reason("None")]
                }))
            },
        );

        let lock_store = DynamoDbLockStore {
            dynamodb: dynamodb_mock,
            table_name: Arc::from(TABLE_NAME),
        };

        let err = lock_store
            .unlock_resources(
                "some-owner",
                /* validate_user */ true,
                RepositoryId::default(),
                &[one_resource()],
            )
            .await
            .expect_err("a cancelled release is not a release");

        assert!(
            matches!(err, LockError::Internal(_)),
            "nothing said the lock was missing, so it must not be reported as missing: {err:?}"
        );
    }
}

/// Another transaction held the item. Nothing about this request is wrong, so it waits its turn.
#[tokio::test(start_paused = true)]
async fn a_lock_entry_lost_to_a_transaction_conflict_backs_off() {
    let mut dynamodb_mock = MockDynamoDb::default();
    dynamodb_mock.expect_transact_write_items().returning(
        move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
            Err(transaction_canceled(vec![reason("TransactionConflict")]))
        },
    );

    let lock_store = DynamoDbLockStore {
        dynamodb: dynamodb_mock,
        table_name: Arc::from(TABLE_NAME),
    };

    let err = lock_store
        .lock_resources("some-owner", RepositoryId::default(), &[one_resource()])
        .await
        .expect_err("a conflicted entry must fail the acquisition");

    assert!(
        matches!(err, LockError::SlowDown(_)),
        "a transaction conflict must be retryable, got {err:?}"
    );
}

/// A release the service sheds as a whole is a back-off.
#[tokio::test(start_paused = true)]
async fn a_shed_lock_release_backs_off() {
    let mut dynamodb_mock = MockDynamoDb::default();
    dynamodb_mock.expect_transact_write_items().returning(
        move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
            Err(service_failure(DynamoErrorCode::RequestLimitExceeded))
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
            RepositoryId::default(),
            &[one_resource()],
        )
        .await
        .expect_err("a shedding table must fail the release");

    assert!(
        matches!(err, LockError::SlowDown(_)),
        "a shed release must be retryable, got {err:?}"
    );
}

/// A shed query is a back-off, which the ladder reconsiders, rather than an internal failure.
#[tokio::test(start_paused = true)]
async fn a_shed_lock_query_backs_off() {
    let mut dynamodb_mock = MockDynamoDb::default();
    dynamodb_mock
        .expect_query_paginated::<LockQuery>()
        .returning(|_, _| {
            Err(service_failure(
                DynamoErrorCode::ProvisionedThroughputExceededException,
            ))
        });

    let lock_store = DynamoDbLockStore {
        dynamodb: dynamodb_mock,
        table_name: Arc::from(TABLE_NAME),
    };

    let err = lock_store
        .query_locks(LockQuery::Repository(RepositoryId::default()))
        .await
        .expect_err("a shedding table must fail the query");

    assert!(
        matches!(err, LockError::SlowDown(_)),
        "a shed query must be retryable, got {err:?}"
    );
}

/// A shed batch read of lock rows is a back-off.
#[tokio::test(start_paused = true)]
async fn a_shed_lock_status_check_backs_off() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_batch_get_item().returning(|_, _, _| {
                Err(service_failure(
                    DynamoErrorCode::ProvisionedThroughputExceededException,
                ))
            });

            let lock_store = DynamoDbLockStore {
                dynamodb: dynamodb_mock,
                table_name: Arc::from(TABLE_NAME),
            };

            let err = lock_store
                .check_locks_status(RepositoryId::default(), &[one_resource()])
                .await
                .expect_err("a shedding table must fail the status check");

            assert!(
                matches!(err, LockError::SlowDown(_)),
                "a shed status check must be retryable, got {err:?}"
            );
        })
        .await;
}

/// A status check whose batch read kept leaving keys unprocessed is a back-off, so the ladder asks
/// again, and the read that follows is the answer.
#[tokio::test(start_paused = true)]
async fn a_lock_status_check_left_unprocessed_is_retried_until_it_succeeds() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let reads = Arc::new(AtomicUsize::new(0));
            let mut dynamodb_mock = MockDynamoDb::default();
            let counted = reads.clone();
            dynamodb_mock
                .expect_batch_get_item()
                .returning(move |_, keys, _| {
                    if counted.fetch_add(1, Ordering::Relaxed) == 0 {
                        Err(AwsError::UnprocessedKeys {
                            remaining: keys.len(),
                        })
                    } else {
                        Ok(Vec::new())
                    }
                });

            let lock_store = DynamoDbLockStore {
                dynamodb: dynamodb_mock,
                table_name: Arc::from(TABLE_NAME),
            };

            let locks = lock_store
                .check_locks_status(RepositoryId::default(), &[one_resource()])
                .await
                .expect("the read after the back-off answers");

            assert!(locks.is_empty());
            assert_eq!(reads.load(Ordering::Relaxed), 2);
        })
        .await;
}

/// A shed acquisition is sent again, and the acquisition that follows is the answer.
#[tokio::test(start_paused = true)]
async fn a_shed_lock_acquisition_is_retried_until_it_succeeds() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let sent = Arc::new(AtomicUsize::new(0));
            let counted = sent.clone();

            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_transact_write_items().returning(
                move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
                    if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(service_failure(
                            DynamoErrorCode::ProvisionedThroughputExceededException,
                        ));
                    }

                    Ok(vec![WriteOutput::default()])
                },
            );

            let lock_store = DynamoDbLockStore {
                dynamodb: dynamodb_mock,
                table_name: Arc::from(TABLE_NAME),
            };

            let resource = one_resource();
            let locks = lock_store
                .lock_resources(
                    "some-owner",
                    RepositoryId::default(),
                    std::slice::from_ref(&resource),
                )
                .await
                .expect("the acquisition after the shed one lands");

            assert_eq!(locks.len(), 1);
            assert_eq!(locks[0].resource.hash, resource.hash);
            assert_eq!(sent.load(Ordering::SeqCst), 2);
        })
        .await;
}

/// An unlock whose answer was lost is sent again under the same request token, so the service
/// answers it with the release it already applied rather than with the locks gone.
#[tokio::test(start_paused = true)]
async fn an_unlock_sent_again_reuses_its_request_token() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let tokens = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = tokens.clone();

            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_transact_write_items().returning(
                move |_: &Arc<str>, _: Vec<TransactWriteItem>, token: Uuid| {
                    let mut recorded = recorded.lock().unwrap();
                    recorded.push(token);
                    if recorded.len() == 1 {
                        return Err(AwsError::Aws(AwsCallError::Timeout));
                    }

                    Ok(vec![WriteOutput::default()])
                },
            );

            let lock_store = DynamoDbLockStore {
                dynamodb: dynamodb_mock,
                table_name: Arc::from(TABLE_NAME),
            };

            lock_store
                .unlock_resources(
                    "some-owner",
                    true,
                    RepositoryId::default(),
                    &[one_resource()],
                )
                .await
                .expect("the unlock sent again is answered");

            let tokens = tokens.lock().unwrap();
            assert_eq!(tokens.len(), 2);
            assert_eq!(tokens[0], tokens[1]);
        })
        .await;
}

/// A lock the caller already holds is dropped from the request and the rest sent again; the answer
/// names only the locks the retry took.
#[tokio::test]
async fn an_acquisition_drops_locks_the_caller_holds_and_retries_the_rest() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let owner_id = "some-owner";
            let repository = RepositoryId::default();
            let held = one_resource();
            let wanted = one_resource();

            let sent = Arc::new(AtomicUsize::new(0));
            let counted = sent.clone();
            let held_row = held.clone();
            let first_token = std::sync::Mutex::new(None);

            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_transact_write_items().returning(
                move |_: &Arc<str>, items: Vec<TransactWriteItem>, token: Uuid| match counted
                    .fetch_add(1, Ordering::SeqCst)
                {
                    0 => {
                        assert_eq!(items.len(), 2);
                        *first_token.lock().unwrap() = Some(token);
                        Err(transaction_canceled(vec![
                            reason_with_lock(
                                "ConditionalCheckFailed",
                                owner_id,
                                repository,
                                held_row.branch,
                                held_row.hash,
                                &held_row.description,
                            ),
                            reason("None"),
                        ]))
                    }
                    1 => {
                        assert_eq!(items.len(), 1, "the held lock is not requested again");
                        assert_ne!(
                            Some(token),
                            *first_token.lock().unwrap(),
                            "a shrunk request is a transaction of its own"
                        );
                        Ok(vec![WriteOutput::default()])
                    }
                    _ => panic!("the shrunk request succeeded, so nothing is sent after it"),
                },
            );

            let lock_store = DynamoDbLockStore {
                dynamodb: dynamodb_mock,
                table_name: Arc::from(TABLE_NAME),
            };

            let locks = lock_store
                .lock_resources(owner_id, repository, &[held.clone(), wanted.clone()])
                .await
                .expect("a held lock is not a conflict");

            assert_eq!(locks.len(), 1);
            assert_eq!(locks[0].resource.hash, wanted.hash);
            assert_eq!(sent.load(Ordering::SeqCst), 2);
        })
        .await;
}

/// A cancellation naming a lock of the caller's that the request did not ask for shrinks nothing,
/// so it is internal and the request is not sent again.
#[tokio::test]
async fn an_acquisition_cancelled_over_a_lock_it_did_not_request_is_internal() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let owner_id = "some-owner";
            let repository = RepositoryId::default();
            let elsewhere = one_resource();

            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_transact_write_items()
                .times(1)
                .return_once(move |_: &Arc<str>, _: Vec<TransactWriteItem>, _: Uuid| {
                    Err(transaction_canceled(vec![reason_with_lock(
                        "ConditionalCheckFailed",
                        owner_id,
                        repository,
                        elsewhere.branch,
                        elsewhere.hash,
                        &elsewhere.description,
                    )]))
                });

            let lock_store = DynamoDbLockStore {
                dynamodb: dynamodb_mock,
                table_name: Arc::from(TABLE_NAME),
            };

            let err = lock_store
                .lock_resources(owner_id, repository, &[one_resource()])
                .await
                .expect_err("a cancellation naming no requested lock is not an acquisition");

            assert!(
                matches!(err, LockError::Internal(_)),
                "expected an internal failure, got {err:?}"
            );
        })
        .await;
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

/// An `Address` encodes as hyphenated hex text while a `Hash` and a `Context` encode as binary,
/// and rows in production hold both spellings. `lore-base` chooses between them by inspecting the
/// serializer's type name, so a change of serializer that got this wrong would rewrite every
/// table's key type in silence — which is what this asserts against.
#[test]
fn test_dynamo_serde_with_addresses() {
    const ADDRESS: &str = "0505050505050505050505050505050505050505050505050505050505050505-\
                           06060606060606060606060606060606";

    let mut binary = Item::new();
    binary.insert(
        ByteStr::from_static("address"),
        AttributeValue::string(ADDRESS),
    );
    binary.insert(
        ByteStr::from_static("hash"),
        AttributeValue::blob([7u8; 32]),
    );
    binary.insert(
        ByteStr::from_static("context"),
        AttributeValue::blob([8u8; 16]),
    );

    let default_item = Foo::default();
    let encoded: Item = to_item(&default_item).unwrap();
    assert_eq!(encoded, binary);

    let decoded: Foo = from_item(binary).unwrap();
    assert_eq!(default_item, decoded);

    // The same row with hash and context written as hex text, which is how older rows hold them.
    let mut textual = Item::new();
    textual.insert(
        ByteStr::from_static("address"),
        AttributeValue::string(ADDRESS),
    );
    textual.insert(
        ByteStr::from_static("hash"),
        AttributeValue::string("0707070707070707070707070707070707070707070707070707070707070707"),
    );
    textual.insert(
        ByteStr::from_static("context"),
        AttributeValue::string("08080808080808080808080808080808"),
    );

    let decoded: Foo = from_item(textual).unwrap();
    assert_eq!(default_item, decoded);
}

fn locked_at(timestamp: &str) -> Option<u64> {
    LockData::try_from(LockEntry {
        timestamp: timestamp.to_string(),
        ..LockEntry::default()
    })
    .ok()
    .map(|lock| lock.locked_at)
}

/// Lock records are shared by servers of every version, so a record's time is read with 0, 3, 6 or
/// 9 fractional digits.
#[test]
fn a_lock_record_time_is_read_as_any_version_writes_it() {
    for timestamp in [
        "2026-11-22T12:45:20.303+00:00",
        "2026-11-22T12:45:20.303350+00:00",
        "2026-11-22T12:45:20.303350755+00:00",
    ] {
        assert_eq!(locked_at(timestamp), Some(1_795_351_520_303), "{timestamp}");
    }
    assert_eq!(
        locked_at("2026-11-22T12:45:20+00:00"),
        Some(1_795_351_520_000)
    );
}

#[test]
fn a_lock_record_time_that_is_not_rfc3339_is_refused() {
    assert_eq!(locked_at("Sun, 22 Nov 2026 12:45:20 +0000"), None);
    assert_eq!(locked_at(""), None);
}
