// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_aws::aws::dynamodb::GetItemOutput;
use lore_aws::aws::dynamodb::QueryOutput;
use lore_aws::aws::dynamodb::WriteOutput;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::error::ErrorPayload;
use lore_aws::aws::error::ServiceError;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::item::to_item;
use lore_aws::aws_error::AwsError;
use lore_aws::dynamodb::ConditionParts;
use lore_aws::dynamodb::DynamoDb;
use lore_aws::dynamodb::DynamoDbCondition;
use lore_aws::dynamodb::DynamoError;
use lore_aws::dynamodb::MockDynamoDb;
use lore_aws::store::mutable_store::AwsMutableStore;
use lore_aws::store::mutable_store::AwsMutableStoreSettings;
use lore_aws::store::mutable_store::CompareAndSwapCondition;
use lore_aws::store::mutable_store::DynamoDbMutableStoreSettings;
use lore_aws::store::mutable_store::ListQuery;
use lore_aws::store::mutable_store::MutableStoreEntry;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_storage::MutableStore;
use mockall::predicate::eq;
use rand::random;

const MUTABLE_STORE_TABLE_NAME: &str = "mutable-store";

/// A failure carrying a service code and no row.
fn service_failure(code: DynamoErrorCode) -> DynamoError {
    AwsError::Aws(lore_aws::aws::error::AwsCallError::service(ServiceError {
        status: http::StatusCode::BAD_REQUEST,
        code,
        message: None,
        request_id: None,
        payload: ErrorPayload::None,
    }))
}

/// A failure the service reports for load, carrying no row.
fn shedding() -> DynamoError {
    service_failure(DynamoErrorCode::ProvisionedThroughputExceededException)
}

/// Failures the service reports for a request it will never accept, carrying no row.
fn refusals() -> [DynamoError; 2] {
    [
        service_failure(DynamoErrorCode::ValidationException),
        service_failure(DynamoErrorCode::AccessDeniedException),
    ]
}

/// The failure a conditional put returns when it loses, carrying the row it lost against.
fn conditional_check_failed(item: Option<Item>) -> DynamoError {
    AwsError::Aws(lore_aws::aws::error::AwsCallError::service(ServiceError {
        status: http::StatusCode::BAD_REQUEST,
        code: DynamoErrorCode::ConditionalCheckFailedException,
        message: None,
        request_id: None,
        payload: match item {
            Some(item) => ErrorPayload::Item(Box::new(item)),
            None => ErrorPayload::None,
        },
    }))
}

fn initialize_mutable_store(dynamodb: DynamoDb) -> AwsMutableStore {
    let settings = AwsMutableStoreSettings::new(DynamoDbMutableStoreSettings::new(
        MUTABLE_STORE_TABLE_NAME.to_string(),
    ));

    AwsMutableStore::new(dynamodb, &settings)
}

/// The `:value` a condition carries.
fn condition_value(parts: &ConditionParts) -> Option<&AttributeValue> {
    parts
        .expression_values
        .iter()
        .find(|(placeholder, _)| *placeholder == ":value")
        .map(|(_, value)| value)
}

#[tokio::test]
async fn test_load_mutable() {
    let hash = random::<Hash>();
    let value = random::<Hash>();
    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::RepositoryId;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);
    let typed_key = MutableStoreEntry::new(repository, typed_hash);

    let item = typed_key.clone().with_value(value);
    let item = to_item(item).unwrap();

    let typed_key: Item = to_item(typed_key).unwrap();

    dynamodb_mock
        .expect_get_item()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(typed_key),
            eq(true),
        )
        .return_once(move |_, _, _| {
            Ok(GetItemOutput {
                item: Some(item),
                request_id: None,
            })
        });

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    assert_eq!(
        value,
        store
            .load(repository.into(), hash, key_type)
            .await
            .expect("failed to load from store")
    );
}

#[tokio::test]
async fn test_load_mutable_not_found() {
    let hash = random::<Hash>();
    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::BranchId;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);
    let typed_key = MutableStoreEntry::new(repository, typed_hash);
    let typed_key: Item = to_item(typed_key).unwrap();
    dynamodb_mock
        .expect_get_item()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(typed_key),
            eq(true),
        )
        .return_once(move |_, _, _| Ok(GetItemOutput::default()));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    assert!(
        store
            .load(repository.into(), hash, key_type)
            .await
            .expect_err("should have gotten an error")
            .is_address_not_found()
    );
}

/// A shed read is asked again, and the answer that follows is the answer.
#[tokio::test(start_paused = true)]
async fn a_shed_load_is_retried_until_it_succeeds() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let repository = random::<Context>();
            let key = random::<Hash>();
            let value = random::<Hash>();
            let row = to_item(
                MutableStoreEntry::new(
                    repository,
                    AwsMutableStore::typed_key(key, KeyType::BranchId),
                )
                .with_value(value),
            )
            .unwrap();

            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_get_item().returning(move |_, _, _| {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(shedding());
                }

                Ok(GetItemOutput {
                    item: Some(row.clone()),
                    request_id: None,
                })
            });

            let loaded = Arc::new(initialize_mutable_store(dynamodb_mock))
                .load(repository.into(), key, KeyType::BranchId)
                .await
                .expect("the read after the shed one answers");

            assert_eq!(loaded, value);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        })
        .await;
}

/// A read the service refuses is internal, and is not asked again.
#[tokio::test(start_paused = true)]
async fn a_load_the_service_refuses_is_internal_after_one_call() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            for refusal in refusals() {
                let mut dynamodb_mock = MockDynamoDb::default();
                dynamodb_mock
                    .expect_get_item()
                    .times(1)
                    .return_once(move |_, _, _| Err(refusal));

                let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                    .load(random::<Context>().into(), random(), KeyType::BranchId)
                    .await
                    .expect_err("a refused read is not an answer");

                assert!(
                    error.is_internal(),
                    "expected an internal failure, got {error:?}"
                );
            }
        })
        .await;
}

/// A read shed past the end of the ladder backs off rather than failing.
#[tokio::test(start_paused = true)]
async fn a_load_shed_past_the_ladder_backs_off() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_get_item()
                .returning(|_, _, _| Err(shedding()));

            let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                .load(random::<Context>().into(), random(), KeyType::BranchId)
                .await
                .expect_err("a read that was never answered is not an answer");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
        })
        .await;
}

/// `list` streams rows out of a channel as pages arrive, paging through `query_page` with the
/// cursor each page names.
#[tokio::test]
async fn test_list_pages_until_the_cursor_is_exhausted() {
    let repository = random::<Context>();
    let key_type = KeyType::BranchId;

    let first = AwsMutableStore::typed_key(random::<Hash>(), key_type);
    let second = AwsMutableStore::typed_key(random::<Hash>(), key_type);
    let values = [random::<Hash>(), random::<Hash>()];
    let first_cursor = to_item(MutableStoreEntry::new(repository, first)).unwrap();

    let mut dynamodb_mock = MockDynamoDb::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();

    dynamodb_mock
        .expect_query_page()
        .returning(move |_, _: ListQuery, start_key| {
            let page = seen.fetch_add(1, Ordering::SeqCst);
            match page {
                0 => {
                    assert_eq!(*start_key, None, "the first page starts at the beginning");
                    Ok(QueryOutput {
                        items: Some(vec![
                            to_item(
                                MutableStoreEntry::new(repository, first).with_value(values[0]),
                            )
                            .unwrap(),
                        ]),
                        last_evaluated_key: Some(first_cursor.clone()),
                        ..QueryOutput::default()
                    })
                }
                1 => {
                    assert_eq!(
                        start_key.as_ref(),
                        Some(&first_cursor),
                        "the second page continues from the cursor the first named"
                    );
                    Ok(QueryOutput {
                        items: Some(vec![
                            to_item(
                                MutableStoreEntry::new(repository, second).with_value(values[1]),
                            )
                            .unwrap(),
                        ]),
                        ..QueryOutput::default()
                    })
                }
                _ => panic!("a listing with no cursor left must stop"),
            }
        });

    let store = Arc::new(initialize_mutable_store(dynamodb_mock));
    let mut channel = store
        .list(repository.into(), key_type)
        .await
        .expect("listing starts")
        .channel();

    let mut listed = Vec::new();
    while let Some((key, value)) = channel.recv().await {
        listed.push((key, value));
    }

    assert_eq!(listed, vec![(first, values[0]), (second, values[1])]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// A shed page waits and asks again, because the alternative is a short answer nobody can detect.
///
/// The stream carries values only, so a listing that stopped early is indistinguishable from one
/// that finished — a caller would read a partial set of branches as the whole set.
#[tokio::test(start_paused = true)]
async fn a_shed_listing_page_is_retried_rather_than_truncating_the_stream() {
    let repository = random::<Context>();
    let key_type = KeyType::BranchId;
    let key = AwsMutableStore::typed_key(random::<Hash>(), key_type);
    let value = random::<Hash>();

    let mut dynamodb_mock = MockDynamoDb::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();

    dynamodb_mock
        .expect_query_page()
        .returning(move |_, _: ListQuery, _| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(shedding());
            }

            Ok(QueryOutput {
                items: Some(vec![
                    to_item(MutableStoreEntry::new(repository, key).with_value(value)).unwrap(),
                ]),
                ..QueryOutput::default()
            })
        });

    let store = Arc::new(initialize_mutable_store(dynamodb_mock));
    let mut channel = store
        .list(repository.into(), key_type)
        .await
        .expect("listing starts")
        .channel();

    let mut listed = Vec::new();
    while let Some(row) = channel.recv().await {
        listed.push(row);
    }

    assert_eq!(
        listed,
        vec![(key, value)],
        "the row behind the shed page must still reach the caller"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the shed page must be asked for again"
    );
}

/// A failure that is not congestion ends the listing after the one request, and the stream ends
/// with it.
///
/// Pinned deliberately: the stream has no way to carry the failure, so this is the shape a caller
/// sees. Giving it an error channel is a `lore-storage` change rather than one this store can make.
#[tokio::test(start_paused = true)]
async fn a_terminal_listing_failure_ends_the_stream() {
    let repository = random::<Context>();
    let key_type = KeyType::BranchId;

    let mut dynamodb_mock = MockDynamoDb::default();
    dynamodb_mock
        .expect_query_page()
        .times(1)
        .returning(move |_, _: ListQuery, _| {
            Err(service_failure(DynamoErrorCode::ValidationException))
        });

    let store = Arc::new(initialize_mutable_store(dynamodb_mock));
    let mut channel = store
        .list(repository.into(), key_type)
        .await
        .expect("listing starts")
        .channel();

    assert!(
        channel.recv().await.is_none(),
        "a listing that cannot proceed ends rather than hanging"
    );
}

/// A page answered with the cursor it resumed from would be asked for again indefinitely, so the
/// listing ends there, having delivered the rows it was given.
#[tokio::test]
async fn a_listing_whose_cursor_stops_advancing_ends() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let repository = random::<Context>();
            let key_type = KeyType::BranchId;
            let key = AwsMutableStore::typed_key(random::<Hash>(), key_type);
            let value = random::<Hash>();
            let cursor = to_item(MutableStoreEntry::new(repository, key)).unwrap();

            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_query_page()
                .times(2)
                .returning(move |_, _: ListQuery, _| {
                    Ok(QueryOutput {
                        items: Some(vec![
                            to_item(MutableStoreEntry::new(repository, key).with_value(value))
                                .unwrap(),
                        ]),
                        last_evaluated_key: Some(cursor.clone()),
                        ..QueryOutput::default()
                    })
                });

            let mut channel = Arc::new(initialize_mutable_store(dynamodb_mock))
                .list(repository.into(), key_type)
                .await
                .expect("listing starts")
                .channel();

            let mut listed = Vec::new();
            while let Some(row) = channel.recv().await {
                listed.push(row);
            }

            assert_eq!(listed, vec![(key, value), (key, value)]);
        })
        .await;
}

/// Rows whose value is zero, or absent, are not listed, and a row that does not decode is skipped
/// rather than ending the listing.
#[tokio::test]
async fn a_listing_skips_zero_values_and_rows_it_cannot_decode() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let repository = random::<Context>();
            let key_type = KeyType::BranchId;
            let listed_key = AwsMutableStore::typed_key(random::<Hash>(), key_type);
            let value = random::<Hash>();

            let mut undecodable = Item::new();
            undecodable.insert(
                ByteStr::from_static("key"),
                AttributeValue::string("not a hash"),
            );

            let page = vec![
                to_item(
                    MutableStoreEntry::new(
                        repository,
                        AwsMutableStore::typed_key(random(), key_type),
                    )
                    .with_value(Hash::default()),
                )
                .unwrap(),
                to_item(MutableStoreEntry::new(
                    repository,
                    AwsMutableStore::typed_key(random(), key_type),
                ))
                .unwrap(),
                undecodable,
                to_item(MutableStoreEntry::new(repository, listed_key).with_value(value)).unwrap(),
            ];

            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_query_page()
                .times(1)
                .return_once(move |_, _: ListQuery, _| {
                    Ok(QueryOutput {
                        items: Some(page),
                        ..QueryOutput::default()
                    })
                });

            let mut channel = Arc::new(initialize_mutable_store(dynamodb_mock))
                .list(repository.into(), key_type)
                .await
                .expect("listing starts")
                .channel();

            let mut listed = Vec::new();
            while let Some(row) = channel.recv().await {
                listed.push(row);
            }

            assert_eq!(listed, vec![(listed_key, value)]);
        })
        .await;
}

/// An untyped listing names no key range, so it is empty without asking the table.
#[tokio::test]
async fn an_untyped_listing_is_empty() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_query_page::<ListQuery>().never();

            let mut channel = Arc::new(initialize_mutable_store(dynamodb_mock))
                .list(random::<Context>().into(), KeyType::Untyped)
                .await
                .expect("listing starts")
                .channel();

            assert!(channel.recv().await.is_none());
        })
        .await;
}

#[tokio::test]
async fn test_store_mutable() {
    let hash = random::<Hash>();
    let value = random::<Hash>();
    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::Untyped;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);

    let item: Item =
        to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value)).unwrap();

    dynamodb_mock
        .expect_put_item()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(item.clone()),
        )
        .return_once(move |_, _| Ok(WriteOutput::default()));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    store
        .store(repository.into(), hash, value, key_type)
        .await
        .expect("should not have returned an error");
}

#[tokio::test]
async fn test_store_mutable_zeroed_value() {
    let hash = random::<Hash>();
    let value = Hash::default();
    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::Untyped;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);
    let item: Item = to_item(MutableStoreEntry::new(repository, typed_hash)).unwrap();

    dynamodb_mock
        .expect_delete_item()
        .with(eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)), eq(item))
        .return_once(move |_, _| Ok(WriteOutput::default()));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    store
        .store(repository.into(), hash, value, key_type)
        .await
        .expect("should not have returned an error");
}

/// A shed write is asked again, and succeeds once the table has room.
#[tokio::test(start_paused = true)]
async fn a_shed_store_is_retried_until_it_succeeds() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock.expect_put_item().returning(move |_, _| {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(shedding());
                }

                Ok(WriteOutput::default())
            });

            Arc::new(initialize_mutable_store(dynamodb_mock))
                .store(
                    random::<Context>().into(),
                    random(),
                    random(),
                    KeyType::BranchId,
                )
                .await
                .expect("the write after the shed one lands");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        })
        .await;
}

/// A write the service refuses is internal, and is not sent again.
#[tokio::test(start_paused = true)]
async fn a_store_the_service_refuses_is_internal_after_one_call() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            for refusal in refusals() {
                let mut dynamodb_mock = MockDynamoDb::default();
                dynamodb_mock
                    .expect_put_item()
                    .times(1)
                    .return_once(move |_, _| Err(refusal));

                let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                    .store(
                        random::<Context>().into(),
                        random(),
                        random(),
                        KeyType::BranchId,
                    )
                    .await
                    .expect_err("a refused write did not land");

                assert!(
                    error.is_internal(),
                    "expected an internal failure, got {error:?}"
                );
            }
        })
        .await;
}

/// A write shed past the end of the ladder backs off rather than failing.
#[tokio::test(start_paused = true)]
async fn a_store_shed_past_the_ladder_backs_off() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_put_item()
                .returning(|_, _| Err(shedding()));

            let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                .store(
                    random::<Context>().into(),
                    random(),
                    random(),
                    KeyType::BranchId,
                )
                .await
                .expect_err("a write that was never accepted did not land");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
        })
        .await;
}

#[tokio::test]
async fn test_compare_and_swap_mutable() {
    let hash = random::<Hash>();
    let value = random::<Hash>();
    let current = random::<Hash>();

    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::RepositoryMetadata;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);

    let item: Item =
        to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value)).unwrap();

    dynamodb_mock
        .expect_put_item_conditional()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(item.clone()),
            eq(CompareAndSwapCondition::new(current)
                .expect("failed to create CompareAndSwap condition")),
        )
        .return_once(move |_, _, _| Ok(WriteOutput::default()));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    assert_eq!(
        current,
        store
            .compare_and_swap(repository.into(), hash, current, value, key_type)
            .await
            .expect("should not have returned an error")
    );
}

#[tokio::test]
async fn test_compare_and_swap_mutable_mismatch() {
    let hash = random::<Hash>();
    let value = random::<Hash>();
    let current = random::<Hash>();
    let expected = random::<Hash>();

    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::BranchId;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);

    let item: Item =
        to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value)).unwrap();

    let actual: Item =
        to_item(MutableStoreEntry::new(repository, typed_hash).with_value(current)).unwrap();

    dynamodb_mock
        .expect_put_item_conditional()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(item.clone()),
            eq(CompareAndSwapCondition::new(expected)
                .expect("failed to create CompareAndSwap condition")),
        )
        .return_once(move |_, _, _| Err(conditional_check_failed(Some(actual))));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    assert_eq!(
        current,
        store
            .compare_and_swap(repository.into(), hash, expected, value, key_type)
            .await
            .expect("should not have returned an error")
    );
}

/// A swap sent again after its answer was lost fails its condition against its own write, and a row
/// already holding the value it writes reports the swap as applied.
#[tokio::test(start_paused = true)]
async fn a_compare_and_swap_that_finds_its_own_value_has_applied() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let hash = random::<Hash>();
            let expected = random::<Hash>();
            let value = random::<Hash>();
            let repository = random::<Context>();
            let key_type = KeyType::BranchId;
            let written: Item = to_item(
                MutableStoreEntry::new(repository, AwsMutableStore::typed_key(hash, key_type))
                    .with_value(value),
            )
            .unwrap();

            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_put_item_conditional::<CompareAndSwapCondition>()
                .returning(move |_, _, _| {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(AwsError::Aws(lore_aws::aws::error::AwsCallError::Timeout));
                    }

                    Err(conditional_check_failed(Some(written.clone())))
                });

            let previous = Arc::new(initialize_mutable_store(dynamodb_mock))
                .compare_and_swap(repository.into(), hash, expected, value, key_type)
                .await
                .expect("its own write is not a lost race");

            assert_eq!(previous, expected);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        })
        .await;
}

#[tokio::test]
async fn test_compare_and_swap_mutable_not_found() {
    let hash = random::<Hash>();
    let value = random::<Hash>();
    let expected = random::<Hash>();

    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::Untyped;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);

    let item: Item =
        to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value)).unwrap();

    dynamodb_mock
        .expect_put_item_conditional()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(item.clone()),
            eq(CompareAndSwapCondition::new(expected)
                .expect("failed to create CompareAndSwap condition")),
        )
        .return_once(move |_, _, _| Err(conditional_check_failed(None)));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    // If we try to compare and swap a non-existent key with an expected value, we should just
    // get back an empty hash.
    assert_eq!(
        Hash::default(),
        store
            .compare_and_swap(repository.into(), hash, expected, value, key_type)
            .await
            .expect("should not have returned an error")
    );
}

/// A zero `expected` matches a row whose value is a stored zero, as well as a row with no value:
/// the condition admits both, and its `:value` is the stored zero's own encoding.
#[test]
fn test_compare_and_swap_zero_expected_with_existing_zero_value() {
    let stored_zero =
        to_item(MutableStoreEntry::new(random(), random::<Hash>()).with_value(Hash::default()))
            .unwrap();

    let parts = CompareAndSwapCondition::new(Hash::default())
        .expect("failed to create CompareAndSwap condition")
        .into_parts();

    assert_eq!(
        parts.condition_expression,
        "attribute_not_exists(#v) OR #v = :value"
    );
    assert_eq!(parts.expression_names.as_slice(), &[("#v", "value")]);
    assert_eq!(condition_value(&parts), stored_zero.get("value"));
}

/// The `:value` a condition compares against is the binary encoding a stored row's `value` has,
/// so the comparison can match.
#[test]
fn a_compare_and_swap_value_is_encoded_as_a_stored_row_encodes_it() {
    let expected = random::<Hash>();
    let stored =
        to_item(MutableStoreEntry::new(random(), random::<Hash>()).with_value(expected)).unwrap();
    let parts = CompareAndSwapCondition::new(expected)
        .expect("failed to create CompareAndSwap condition")
        .into_parts();

    assert_eq!(parts.condition_expression, "#v = :value");
    assert_eq!(condition_value(&parts), stored.get("value"));
    assert_eq!(
        condition_value(&parts).and_then(AttributeValue::as_blob),
        Some(expected.data().as_slice())
    );
}

#[tokio::test]
async fn test_compare_and_swap_mutable_not_found_expected() {
    let hash = random::<Hash>();
    let value = random::<Hash>();
    let expected = Hash::default();

    let repository = random::<Context>();

    let mut dynamodb_mock = MockDynamoDb::default();

    let key_type = KeyType::Untyped;
    let typed_hash = AwsMutableStore::typed_key(hash, key_type);

    let item: Item =
        to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value)).unwrap();

    dynamodb_mock
        .expect_put_item_conditional()
        .with(
            eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
            eq(item.clone()),
            eq(CompareAndSwapCondition::new(expected)
                .expect("failed to create CompareAndSwap condition")),
        )
        .return_once(move |_, _, _| Ok(WriteOutput::default()));

    let store = initialize_mutable_store(dynamodb_mock);
    let store = Arc::new(store);

    // If we try to compare and swap a non-existent key with an empty expected value, we should
    // perform the write and get back the written value.
    assert_eq!(
        expected,
        store
            .compare_and_swap(repository.into(), hash, expected, value, key_type)
            .await
            .expect("should not have returned an error")
    );
}

/// A zero `expected` matches a missing value, so a lost condition with no row behind it cannot
/// be explained and is internal.
#[tokio::test]
async fn a_compare_and_swap_from_zero_lost_without_a_row_is_internal() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_put_item_conditional::<CompareAndSwapCondition>()
                .times(1)
                .return_once(|_, _, _| Err(conditional_check_failed(None)));

            let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                .compare_and_swap(
                    random::<Context>().into(),
                    random(),
                    Hash::default(),
                    random(),
                    KeyType::BranchId,
                )
                .await
                .expect_err("a lost condition with nothing behind it is not a swap");

            assert!(
                error.is_internal(),
                "expected an internal failure, got {error:?}"
            );
        })
        .await;
}

/// A shed compare-and-swap is sent again, and the swap that follows is the answer.
#[tokio::test(start_paused = true)]
async fn a_shed_compare_and_swap_is_retried_until_it_succeeds() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let expected = random::<Hash>();
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_put_item_conditional::<CompareAndSwapCondition>()
                .returning(move |_, _, _| {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(shedding());
                    }

                    Ok(WriteOutput::default())
                });

            let previous = Arc::new(initialize_mutable_store(dynamodb_mock))
                .compare_and_swap(
                    random::<Context>().into(),
                    random(),
                    expected,
                    random(),
                    KeyType::BranchId,
                )
                .await
                .expect("the swap after the shed one lands");

            assert_eq!(previous, expected);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        })
        .await;
}

/// A compare-and-swap the service refuses is internal, and is not sent again.
#[tokio::test(start_paused = true)]
async fn a_compare_and_swap_the_service_refuses_is_internal_after_one_call() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            for refusal in refusals() {
                let mut dynamodb_mock = MockDynamoDb::default();
                dynamodb_mock
                    .expect_put_item_conditional::<CompareAndSwapCondition>()
                    .times(1)
                    .return_once(move |_, _, _| Err(refusal));

                let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                    .compare_and_swap(
                        random::<Context>().into(),
                        random(),
                        random(),
                        random(),
                        KeyType::BranchId,
                    )
                    .await
                    .expect_err("a refused swap did not land");

                assert!(
                    error.is_internal(),
                    "expected an internal failure, got {error:?}"
                );
            }
        })
        .await;
}

/// A compare-and-swap shed past the end of the ladder backs off rather than failing.
#[tokio::test(start_paused = true)]
async fn a_compare_and_swap_shed_past_the_ladder_backs_off() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_put_item_conditional::<CompareAndSwapCondition>()
                .returning(|_, _, _| Err(shedding()));

            let error = Arc::new(initialize_mutable_store(dynamodb_mock))
                .compare_and_swap(
                    random::<Context>().into(),
                    random(),
                    random(),
                    random(),
                    KeyType::BranchId,
                )
                .await
                .expect_err("a swap that was never accepted did not land");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
        })
        .await;
}

const SWAP_KEY_TYPE: KeyType = KeyType::BranchLatestPointer;

/// The item key of `hash` in `repository`, with no value.
fn swap_item_key(repository: Context, hash: Hash) -> Item {
    to_item(MutableStoreEntry::new(
        repository,
        AwsMutableStore::typed_key(hash, SWAP_KEY_TYPE),
    ))
    .unwrap()
}

/// The item of `hash` in `repository` holding `value`.
fn swap_item(repository: Context, hash: Hash, value: Hash) -> Item {
    to_item(
        MutableStoreEntry::new(repository, AwsMutableStore::typed_key(hash, SWAP_KEY_TYPE))
            .with_value(value),
    )
    .unwrap()
}

/// What a swap of `hash` in `repository` from `expected` to `value` answers through `dynamodb`.
async fn swap(
    dynamodb: DynamoDb,
    repository: Context,
    hash: Hash,
    expected: Hash,
    value: Hash,
) -> Result<Hash, lore_storage::StoreError> {
    Arc::new(initialize_mutable_store(dynamodb))
        .compare_and_swap(repository.into(), hash, expected, value, SWAP_KEY_TYPE)
        .await
}

/// A swap to zero deletes the item on the swap's condition, whether it swaps from a value or
/// from zero, and answers `expected` when the delete is made.
#[tokio::test]
async fn a_swap_to_zero_deletes_the_item_on_its_condition() {
    for expected in [random::<Hash>(), Hash::default()] {
        let (hash, repository) = (random::<Hash>(), random::<Context>());
        let mut dynamodb_mock = MockDynamoDb::default();
        dynamodb_mock
            .expect_delete_item_conditional()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(swap_item_key(repository, hash)),
                eq(CompareAndSwapCondition::new(expected).unwrap()),
            )
            .times(1)
            .return_once(|_, _, _| Ok(WriteOutput::default()));

        assert_eq!(
            swap(dynamodb_mock, repository, hash, expected, Hash::default())
                .await
                .expect("swaps"),
            expected,
            "from {expected}"
        );
    }
}

/// A refused swap to zero answers the value of the item `DynamoDB` returns with the refusal.
#[tokio::test]
async fn a_refused_swap_to_zero_answers_the_held_value() {
    let (hash, repository) = (random::<Hash>(), random::<Context>());
    let (expected, held) = (random::<Hash>(), random::<Hash>());
    let returned = swap_item(repository, hash, held);
    let mut dynamodb_mock = MockDynamoDb::default();
    dynamodb_mock
        .expect_delete_item_conditional::<CompareAndSwapCondition>()
        .return_once(move |_, _, _| Err(conditional_check_failed(Some(returned))));

    assert_eq!(
        swap(dynamodb_mock, repository, hash, expected, Hash::default())
            .await
            .expect("swaps"),
        held
    );
}

/// A refused swap answers zero when `DynamoDB` returns no item with the refusal, or an item with
/// no value, by a put and by a delete alike.
#[tokio::test]
async fn a_refused_swap_answers_zero_for_no_item_or_an_item_without_a_value() {
    for to_zero in [false, true] {
        for valueless in [false, true] {
            let (hash, repository) = (random::<Hash>(), random::<Context>());
            let returned = valueless.then(|| swap_item_key(repository, hash));
            let mut dynamodb_mock = MockDynamoDb::default();
            if to_zero {
                dynamodb_mock
                    .expect_delete_item_conditional::<CompareAndSwapCondition>()
                    .return_once(move |_, _, _| Err(conditional_check_failed(returned)));
            } else {
                dynamodb_mock
                    .expect_put_item_conditional::<CompareAndSwapCondition>()
                    .return_once(move |_, _, _| Err(conditional_check_failed(returned)));
            }
            let value = if to_zero {
                Hash::default()
            } else {
                random::<Hash>()
            };

            assert_eq!(
                swap(dynamodb_mock, repository, hash, random::<Hash>(), value)
                    .await
                    .expect("swaps"),
                Hash::default(),
                "to zero {to_zero}, an item without a value {valueless}"
            );
        }
    }
}

/// A refusal whose returned item meets the swap's condition contradicts it, so the swap fails
/// rather than answer `expected`, which would read as made: by a put and by a delete, from a value
/// or from zero with no item returned.
#[tokio::test]
async fn a_refusal_that_answers_the_expected_value_fails_the_swap() {
    for to_zero in [false, true] {
        for from_zero in [false, true] {
            let (hash, repository) = (random::<Hash>(), random::<Context>());
            let expected = if from_zero {
                Hash::default()
            } else {
                random::<Hash>()
            };
            let returned = (!from_zero).then(|| swap_item(repository, hash, expected));
            let mut dynamodb_mock = MockDynamoDb::default();
            let value = if to_zero {
                dynamodb_mock
                    .expect_delete_item_conditional::<CompareAndSwapCondition>()
                    .return_once(move |_, _, _| Err(conditional_check_failed(returned)));
                Hash::default()
            } else {
                dynamodb_mock
                    .expect_put_item_conditional::<CompareAndSwapCondition>()
                    .return_once(move |_, _, _| Err(conditional_check_failed(returned)));
                random::<Hash>()
            };

            let swapped = swap(dynamodb_mock, repository, hash, expected, value).await;
            assert!(
                swapped.as_ref().is_err_and(|err| err.is_internal()),
                "to zero {to_zero}, from zero {from_zero}: answered {swapped:?}"
            );
        }
    }
}

/// A swap to zero the service refuses other than on its condition is internal, and is not sent
/// again.
#[tokio::test(start_paused = true)]
async fn a_swap_to_zero_the_service_refuses_is_internal_after_one_call() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            for refusal in refusals() {
                let mut dynamodb_mock = MockDynamoDb::default();
                dynamodb_mock
                    .expect_delete_item_conditional::<CompareAndSwapCondition>()
                    .times(1)
                    .return_once(move |_, _, _| Err(refusal));

                let error = swap(dynamodb_mock, random(), random(), random(), Hash::default())
                    .await
                    .expect_err("a refused delete did not land");

                assert!(
                    error.is_internal(),
                    "expected an internal failure, got {error:?}"
                );
            }
        })
        .await;
}

/// A swap to zero shed past the end of the ladder backs off rather than failing, as a put does.
#[tokio::test(start_paused = true)]
async fn a_swap_to_zero_shed_past_the_ladder_backs_off() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let mut dynamodb_mock = MockDynamoDb::default();
            dynamodb_mock
                .expect_delete_item_conditional::<CompareAndSwapCondition>()
                .returning(move |_, _, _| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err(shedding())
                });

            let error = swap(dynamodb_mock, random(), random(), random(), Hash::default())
                .await
                .expect_err("a delete that was never accepted did not land");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
            assert!(
                calls.load(Ordering::SeqCst) > 1,
                "a shed delete is sent again"
            );
        })
        .await;
}

/// A swap from the held value to itself reads the item and writes nothing, answering what the item
/// holds: the value, another value, or zero for no item.
#[tokio::test]
async fn a_swap_to_the_expected_value_reads_and_writes_nothing() {
    let value = random::<Hash>();
    for held in [Some(value), Some(random::<Hash>()), None] {
        let (hash, repository) = (random::<Hash>(), random::<Context>());
        let returned = held.map(|held| swap_item(repository, hash, held));
        let mut dynamodb_mock = MockDynamoDb::default();
        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(swap_item_key(repository, hash)),
                eq(true),
            )
            .times(1)
            .return_once(move |_, _, _| {
                Ok(GetItemOutput {
                    item: returned,
                    request_id: None,
                })
            });

        assert_eq!(
            swap(dynamodb_mock, repository, hash, value, value)
                .await
                .expect("swaps"),
            held.unwrap_or_default()
        );
    }
}
