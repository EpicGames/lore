// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_smithy_types::Blob;
use lore_base::error::AddressNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_storage::ImmutableStore;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore as MutableStoreTrait;
use lore_storage::StoreError;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use opentelemetry::KeyValue;
use serde::Deserialize;
use serde::Serialize;
use smallvec::SmallVec;
use tracing::Instrument;
use tracing::debug;
use tracing::info;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::aws_error::AwsError;
use crate::default_aws_timeout_millis;
use crate::dynamodb::ConditionParts;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbPutCondition;
use crate::dynamodb::DynamoDbQuery;

pub const MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE: &str = "repository_id";
pub const MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE: &str = "key";
pub const MUTABLE_STORE_VALUE_ATTRIBUTE: &str = "value";

static STORE_ATTRIBUTES: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("store", "aws")]);

#[derive(Clone, Debug, Deserialize)]
pub struct DynamoDbMutableStoreSettings {
    pub mutable_store_table_name: String,
    pub endpoint_url: Option<String>,
    pub region: Option<String>,
    pub slow_operation_threshold_millis: u64,
    #[serde(default = "default_aws_timeout_millis")]
    pub timeout_millis: u64,
}

impl DynamoDbMutableStoreSettings {
    pub fn new(mutable_store_table_name: String) -> Self {
        Self {
            mutable_store_table_name,
            endpoint_url: None,
            region: None,
            slow_operation_threshold_millis: u64::MAX,
            timeout_millis: default_aws_timeout_millis(),
        }
    }

    pub fn with_endpoint(mut self, endpoint_url: String) -> Self {
        self.endpoint_url = Some(endpoint_url);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MutableStoreEntry {
    repository_id: Context,
    key: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Hash>,
}

impl MutableStoreEntry {
    fn new(repository_id: Context, key: Hash) -> Self {
        Self {
            repository_id,
            key,
            value: None,
        }
    }

    fn with_value(mut self, value: Hash) -> Self {
        self.value = Some(value);
        self
    }
}

#[derive(Debug, PartialEq)]
struct CompareAndSwapCondition {
    condition_expression: String,
    expression_names: HashMap<String, String>,
    expression_values: HashMap<String, AttributeValue>,
}

impl CompareAndSwapCondition {
    fn new(expected: Hash) -> Result<Self, StoreError> {
        const VALUE_ATTRIBUTE_ALIAS: &str = "#v";
        const VALUE_PLACEHOLDER: &str = ":value";

        let mut expression_names: HashMap<String, String> = HashMap::new();
        let mut expression_values: HashMap<String, AttributeValue> = HashMap::new();

        let expected_value: AttributeValue = serde_dynamo::to_attribute_value(expected).map_err(|e| {
            warn!("Failed to convert expected value: {expected:?} to dynamo attribute value map: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize expected hash for DynamoDB condition")
        })?;

        expression_names.insert(
            VALUE_ATTRIBUTE_ALIAS.to_string(),
            MUTABLE_STORE_VALUE_ATTRIBUTE.to_string(),
        );
        expression_values.insert(VALUE_PLACEHOLDER.to_string(), expected_value);

        // When expected is zero the caller treats "absent" and "stored zero" as equivalent
        // starting states. The condition must match both:
        //   - the value attribute is missing entirely (row was never written, or was deleted)
        //   - the value attribute is present but equals zero (e.g. written by branch initialization)
        //
        // The previous condition (`attribute_not_exists(pk) AND attribute_not_exists(sk)`)
        // only matched the first case. When E.G. `branch::create` is called for a default branch
        // with no commits it writes {value=0000…} via compare_and_swap(expected=0, value=0),
        // creating the row. Subsequent pushes then call compare_and_swap(expected=0, value=X)
        // and found the condition failing (row exists), DynamoDB returning the existing
        // {value=0000…} via AllOld, and the code incorrectly interpreting previous==expected
        // as a successful swap — silently dropping the write.
        let condition_expression = if expected.is_zero() {
            format!(
                "attribute_not_exists({VALUE_ATTRIBUTE_ALIAS}) OR {VALUE_ATTRIBUTE_ALIAS} = {VALUE_PLACEHOLDER}"
            )
        } else {
            format!("{VALUE_ATTRIBUTE_ALIAS} = {VALUE_PLACEHOLDER}")
        };

        Ok(Self {
            condition_expression,
            expression_names,
            expression_values,
        })
    }
}

impl DynamoDbPutCondition for CompareAndSwapCondition {
    fn into_parts(self) -> ConditionParts {
        ConditionParts {
            condition_expression: self.condition_expression,
            expression_names: self.expression_names,
            expression_values: self.expression_values,
        }
    }
}

struct ListQuery {
    repository: Context,
    key_type: KeyType,
}

impl DynamoDbQuery for ListQuery {
    fn key_condition_expression(&self) -> &str {
        "#repo = :repo AND #key BETWEEN :ks AND :ke"
    }

    fn expression_attribute_names(&self) -> HashMap<String, String> {
        HashMap::from([
            ("#repo".to_string(), "repository_id".to_string()),
            ("#key".to_string(), "key".to_string()),
        ])
    }

    fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
        // Construct the start and end values to include all entries of a given type
        let mut key_start = [0u8; 32];
        key_start[0] = self.key_type as u8;
        let mut key_end = [0xFFu8; 32];
        key_end[0] = self.key_type as u8;

        HashMap::from([
            (
                ":repo".to_string(),
                AttributeValue::B(Blob::from(self.repository.as_bytes())),
            ),
            (
                ":ks".to_string(),
                AttributeValue::B(Blob::from(key_start.as_slice())),
            ),
            (
                ":ke".to_string(),
                AttributeValue::B(Blob::from(key_end.as_slice())),
            ),
        ])
    }

    fn filter_expression(&self) -> Option<String> {
        None
    }

    fn consistent_read(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct AwsMutableStoreSettings {
    pub dynamodb: DynamoDbMutableStoreSettings,
    #[serde(default)]
    pub force_write: bool,
}

impl AwsMutableStoreSettings {
    pub fn new(dynamodb: DynamoDbMutableStoreSettings, force_write: bool) -> Self {
        Self {
            dynamodb,
            force_write,
        }
    }
}

pub struct AwsMutableStore {
    dynamodb: DynamoDb,
    mutable_store_table_name: Arc<str>,
}

impl AwsMutableStore {
    #[allow(unused)]
    pub fn new(
        dynamodb: DynamoDb,
        settings: &AwsMutableStoreSettings,
        immutable_store: Arc<dyn ImmutableStore>,
    ) -> Self {
        info!("Creating AWS mutable store");
        Self {
            dynamodb,
            mutable_store_table_name: Arc::from(settings.dynamodb.mutable_store_table_name.clone()),
        }
    }

    fn typed_key(mut key: Hash, key_type: KeyType) -> Hash {
        key.data_mut()[0] = key_type as u8;
        key
    }

    #[tracing::instrument(name= "AwsMutableStore::load_raw" skip(self))]
    async fn load_typed(
        self: Arc<Self>,
        repository: Context,
        key: Hash,
    ) -> Result<Hash, StoreError> {
        let entry = MutableStoreEntry::new(repository, key);
        let key_item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to convert mutable store entry: {entry:?} to dynamo attribute value map: {e:?}");
            StoreError::internal_with_context(e, "failed to convert mutable store entry {entry:?} to dynamo attribute value map")
        })?;

        let response = self
            .dynamodb
            .get_item(
                &self.mutable_store_table_name,
                key_item,
                true, /* consistent read */
            )
            .await;

        match response {
            Ok(output) if output.item().is_some() => {
                let av_map = output.item().unwrap();
                let item: MutableStoreEntry = serde_dynamo::from_item(av_map.to_owned()).map_err(|e| {
                    warn!("Failed to deserialize dynamodb item: {av_map:?} into mutable store entry: {e:?}");
                    StoreError::internal_with_context(e, "failed to deserialize dynamodb item into mutable store entry")
                })?;
                if let Some(value) = item.value
                    // match the local store implementation - if the value is zero
                    // then treat as AddressNotFound
                    && !value.is_zero()
                {
                    Ok(value)
                } else {
                    Err(StoreError::from(AddressNotFound::from(
                        Address::zero_context_hash(key),
                    )))
                }
            }
            Ok(_) => Err(StoreError::from(AddressNotFound::from(
                Address::zero_context_hash(key),
            ))),
            Err(err) => {
                warn!(
                    "Got unexpected error from AWS sdk while loading {key} from the mutable store for repository: {repository}: {err:?}"
                );
                Err(match err {
                    AwsError::AwsSdkError(_sdk_error) => StoreError::from(SlowDown),
                    _ => StoreError::internal_with_context(
                        err,
                        "Got unexpected error from AWS sdk while loading {key} from the mutable store for repository: {repository}",
                    ),
                })
            }
        }
    }

    async fn store_typed(
        self: Arc<Self>,
        repository: Context,
        key: Hash,
        value: Hash,
    ) -> Result<(), StoreError> {
        let entry = MutableStoreEntry::new(repository, key);
        if value.is_zero() {
            let item = serde_dynamo::to_item(&entry).map_err(|e| {
                warn!("Failed to convert mutable store entry: {entry:?} to dynamo attribute value map: {e:?}");
                StoreError::internal_with_context(e, "Failed to convert mutable store entry: {entry:?} to dynamo attribute value map")
            })?;

            self.dynamodb.delete_item(&self.mutable_store_table_name, item).await.map(|_| ())
                .map_err(|err| {
                    warn!("Failed to delete item while storing empty value to mutable store for key: {key} in repository: {repository}: {err:?}");
                    match err {
                        AwsError::AwsSdkError(_sdk_error) => StoreError::from(SlowDown),
                        _ => StoreError::internal_with_context(err, "Failed to delete item while storing empty value to mutable store for key: {key} in repository: {repository}: {err:?}"),
                    }
                })
        } else {
            let entry = entry.with_value(value);
            let item = serde_dynamo::to_item(&entry).map_err(|e| {
                warn!("Failed to convert mutable store entry: {entry:?} to dynamo attribute value map: {e:?}");
                StoreError::internal_with_context(e, "Failed to serialize mutable entry for DynamoDB compare-and-swap")
            })?;
            self.dynamodb
                .put_item(&self.mutable_store_table_name, item)
                .await
                .map(|_| ())
                .map_err(|err| {
                    debug!({REPOSITORY_ID} = %repository, key = %key, error = ?err,
                        "Failed to put item while storing mutable value for key");
                    match err {
                        AwsError::AwsSdkError(_sdk_error) => StoreError::from(SlowDown),
                        _ => StoreError::internal_with_context(
                            err,
                            "Failed to put item while storing mutable value for key",
                        ),
                    }
                })
        }
    }

    async fn compare_and_swap_typed(
        self: Arc<Self>,
        repository: Context,
        key: Hash,
        expected: Hash,
        value: Hash,
    ) -> Result<Hash, StoreError> {
        let entry = MutableStoreEntry::new(repository, key).with_value(value);

        let item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to convert mutable store entry: {entry:?} to dynamo attribute value map: {e:?}");
            StoreError::internal_with_context(e, "failed to convert mutable store entry {entry:?} to dynamo attribute value map")
        })?;

        let result = self
            .dynamodb
            .put_item_conditional(
                &self.mutable_store_table_name,
                item,
                CompareAndSwapCondition::new(expected)?,
            )
            .await;

        match result {
            Ok(_) => Ok(expected),
            Err(AwsError::AwsSdkError(sdk_error))
                if sdk_error
                    .as_service_error()
                    .is_some_and(|e| e.is_conditional_check_failed_exception()) =>
            {
                if let Some(PutItemError::ConditionalCheckFailedException(e)) =
                    sdk_error.as_service_error()
                {
                    match e.item() {
                        Some(item) => {
                            let entry: MutableStoreEntry =
                                    serde_dynamo::from_item(item.to_owned()).map_err(|e| {
                                        warn!("Failed to parse mutable store value from item: {item:?}: {e}");
                                        StoreError::internal_with_context(e, "Failed to convert mutable store value from item: {item:?}")
                                    })?;
                            entry.value.ok_or_else(|| {
                                warn!("Could not extract value from existing item");
                                StoreError::internal("Could not extract value from existing item")
                            })
                        }
                        None => {
                            if expected.is_zero() {
                                // We expected no value to exist, and the request failed the
                                // precondition, but returned no existing item. This shouldn't ever
                                // happen.
                                warn!(
                                    "Precondition failed when compare and swap to insert new value, but no existing value found?"
                                );
                                Err(StoreError::internal(
                                    "Precondition failed when compare and swap to insert new value, but no existing value found?",
                                ))
                            } else {
                                // We expected a value to exist, the precondition failed with no
                                // value, meaning the item did not exist at all, so we just return
                                // an empty hash.
                                Ok(Hash::default())
                            }
                        }
                    }
                } else {
                    unreachable!()
                }
            }
            Err(err) => {
                warn!("DynamoDB conditional put failed for compare and swap of {entry:?}: {err:?}");
                Err(match err {
                    AwsError::AwsSdkError(_sdk_error) => StoreError::from(SlowDown),
                    _ => StoreError::internal_with_context(
                        err,
                        "DynamoDB conditional put failed for compare and swap of {entry:?}",
                    ),
                })
            }
        }
    }

    fn list_typed(
        self: Arc<Self>,
        repository: Context,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        let (stream, sender) = KeyValueStream::new();

        if key_type == KeyType::Untyped {
            return Ok(stream);
        }

        let query = ListQuery {
            repository,
            key_type,
        };
        let table_name = self.mutable_store_table_name.clone();
        let client = self.dynamodb.sdk_client().clone();

        lore_base::lore_spawn!(async move {
            let attribute_names = query.expression_attribute_names();
            let attribute_values = query.expression_attribute_values();
            let mut last_evaluated_key = None;
            loop {
                let result = client
                    .query()
                    .table_name(&*table_name)
                    .consistent_read(query.consistent_read())
                    .key_condition_expression(query.key_condition_expression())
                    .set_expression_attribute_names(Some(attribute_names.clone()))
                    .set_expression_attribute_values(Some(attribute_values.clone()))
                    .set_exclusive_start_key(last_evaluated_key)
                    .send()
                    .await;

                match result {
                    Ok(output) => {
                        if let Some(items) = output.items {
                            for item in items {
                                if let Ok(entry) = serde_dynamo::from_item::<_, MutableStoreEntry>(
                                    item,
                                )
                                .map_err(|e| {
                                    warn!("Error converting DDB item to MutableStoreEntry: {e}");
                                }) && let Some(value) = entry.value
                                    && !value.is_zero()
                                    && let Err(err) = sender.send((entry.key, value))
                                {
                                    debug!(err = %err, "Failed sending mutable list result");
                                    return;
                                }
                            }
                        }

                        match output.last_evaluated_key {
                            Some(key) if !key.is_empty() => {
                                last_evaluated_key = Some(key);
                            }
                            _ => break,
                        }
                    }
                    Err(err) => {
                        warn!(error = ?err, "Error querying DDB for list");
                        break;
                    }
                }
            }
        }.in_current_span());

        Ok(stream)
    }
}

#[async_trait]
impl MutableStoreTrait for AwsMutableStore {
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsMutableStore::list" skip(self))]
    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        let repository: Context = partition.into();
        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("list"),
            { self.clone().list_typed(repository, key_type) }
        )
        .into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsMutableStore::load" skip(self))]
    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let repository: Context = partition.into();
        let typed_key = Self::typed_key(key, key_type);

        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("load"),
            { self.clone().load_typed(repository, typed_key).await }
        )
        .into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsMutableStore::store" skip(self))]
    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        let repository: Context = partition.into();
        let typed_key = Self::typed_key(key, key_type);

        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("store"),
            { self.clone().store_typed(repository, typed_key, value).await }
        )
        .into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsMutableStore::compare_and_swap" skip(self))]
    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let repository: Context = partition.into();
        let typed_key = Self::typed_key(key, key_type);

        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("compare_and_swap"),
            {
                self.clone()
                    .compare_and_swap_typed(repository, typed_key, expected, value)
                    .await
            }
        )
        .into()
    }

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        // Noop for AWS
        Ok(())
    }
}

impl InstrumentProvider for AwsMutableStore {
    fn namespace(&self) -> &'static str {
        "urc.store.mutable.aws"
    }

    fn labels(&self) -> &[KeyValue] {
        STORE_ATTRIBUTES.as_slice()
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::sync::Arc;

    use aws_sdk_dynamodb::operation::delete_item::DeleteItemOutput;
    use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
    use aws_sdk_dynamodb::operation::put_item::PutItemError;
    use aws_sdk_dynamodb::operation::put_item::PutItemOutput;
    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_sdk_dynamodb::types::error::ConditionalCheckFailedException;
    use aws_sdk_s3::primitives::SdkBody;
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::client::result::SdkError;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_storage::MutableStore;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use mockall::predicate::eq;
    use rand::random;

    use crate::aws_error::AwsError;
    use crate::dynamodb::DynamoDb;
    use crate::dynamodb::MockDynamoDb;
    use crate::store::mutable_store::AwsMutableStore;
    use crate::store::mutable_store::AwsMutableStoreSettings;
    use crate::store::mutable_store::CompareAndSwapCondition;
    use crate::store::mutable_store::DynamoDbMutableStoreSettings;
    use crate::store::mutable_store::MutableStoreEntry;

    const MUTABLE_STORE_TABLE_NAME: &str = "mutable-store";

    async fn initialize_mutable_store(dynamodb: DynamoDb) -> AwsMutableStore {
        let immutable_store = lore_storage::local::immutable_store::LocalImmutableStore::new(
            None,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create temporary immutable store");

        let settings = AwsMutableStoreSettings {
            dynamodb: DynamoDbMutableStoreSettings::new(MUTABLE_STORE_TABLE_NAME.to_string()),
            force_write: false,
        };

        AwsMutableStore::new(dynamodb, &settings, immutable_store)
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
        let item = serde_dynamo::to_item(item).unwrap();

        let typed_key: HashMap<String, AttributeValue> = serde_dynamo::to_item(typed_key).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(typed_key),
                eq(true),
            )
            .return_once(move |_, _, _| Ok(GetItemOutput::builder().set_item(Some(item)).build()));

        let store = initialize_mutable_store(dynamodb_mock).await;
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
        let typed_key: HashMap<String, AttributeValue> = serde_dynamo::to_item(typed_key).unwrap();
        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(typed_key),
                eq(true),
            )
            .return_once(move |_, _, _| Ok(GetItemOutput::builder().set_item(None).build()));

        // Expect the fallback read
        let fallback_key = MutableStoreEntry::new(repository, hash);
        let fallback_key: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(fallback_key).unwrap();
        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(fallback_key),
                eq(true),
            )
            .return_once(move |_, _, _| Ok(GetItemOutput::builder().set_item(None).build()));

        let store = initialize_mutable_store(dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .load(repository.into(), hash, key_type)
                .await
                .expect_err("should have gotten an error")
                .is_address_not_found()
        );
    }

    #[tokio::test]
    async fn test_store_mutable() {
        let hash = random::<Hash>();
        let value = random::<Hash>();
        let repository = random::<Context>();

        let mut dynamodb_mock = MockDynamoDb::default();

        let key_type = KeyType::Untyped;
        let typed_hash = AwsMutableStore::typed_key(hash, key_type);

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value))
                .unwrap();

        dynamodb_mock
            .expect_put_item()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(item.clone()),
            )
            .return_once(move |_, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        let store = initialize_mutable_store(dynamodb_mock).await;
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
        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash)).unwrap();

        dynamodb_mock
            .expect_delete_item()
            .with(eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)), eq(item))
            .return_once(move |_, _| Ok(DeleteItemOutput::builder().build()));

        let store = initialize_mutable_store(dynamodb_mock).await;
        let store = Arc::new(store);

        store
            .store(repository.into(), hash, value, key_type)
            .await
            .expect("should not have returned an error");
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

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value))
                .unwrap();

        dynamodb_mock
            .expect_put_item_conditional()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(item.clone()),
                eq(CompareAndSwapCondition::new(current)
                    .expect("failed to create CompareAndSwap condition")),
            )
            .return_once(move |_, _, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        let store = initialize_mutable_store(dynamodb_mock).await;
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

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value))
                .unwrap();

        let actual: HashMap<String, AttributeValue> = serde_dynamo::to_item(
            MutableStoreEntry::new(repository, typed_hash).with_value(current),
        )
        .unwrap();

        dynamodb_mock
            .expect_put_item_conditional()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(item.clone()),
                eq(CompareAndSwapCondition::new(expected)
                    .expect("failed to create CompareAndSwap condition")),
            )
            .return_once(move |_, _, _| {
                Err(AwsError::sdk_error(SdkError::ServiceError(
                    aws_smithy_runtime_api::client::result::ServiceError::builder()
                        .source(PutItemError::ConditionalCheckFailedException(
                            ConditionalCheckFailedException::builder()
                                .set_item(Some(actual))
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            404u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            });

        let store = initialize_mutable_store(dynamodb_mock).await;
        let store = Arc::new(store);

        assert_eq!(
            current,
            store
                .compare_and_swap(repository.into(), hash, expected, value, key_type)
                .await
                .expect("should not have returned an error")
        );
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

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value))
                .unwrap();

        dynamodb_mock
            .expect_put_item_conditional()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(item.clone()),
                eq(CompareAndSwapCondition::new(expected)
                    .expect("failed to create CompareAndSwap condition")),
            )
            .return_once(move |_, _, _| {
                Err(AwsError::sdk_error(SdkError::ServiceError(
                    aws_smithy_runtime_api::client::result::ServiceError::builder()
                        .source(PutItemError::ConditionalCheckFailedException(
                            ConditionalCheckFailedException::builder()
                                .set_item(None)
                                .build(),
                        ))
                        .raw(HttpResponse::new(
                            404u16.try_into().unwrap(),
                            SdkBody::empty(),
                        ))
                        .build(),
                )))
            });

        let store = initialize_mutable_store(dynamodb_mock).await;
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

    /// Regression test: pushing to a default branch whose LATEST key was initialised to
    /// {value=0000…} by `branch::create` must succeed, not silently no-op.
    ///
    /// Previously the zero-expected CAS condition was `attribute_not_exists(pk) AND
    /// attribute_not_exists(sk)`, which fails when the row already exists. `DynamoDB` then
    /// returns the existing item via `AllOld`; the code saw `previous == expected == 0` and
    /// incorrectly treated the failed swap as a success, leaving the branch pointer unadvanced.
    #[tokio::test]
    async fn test_compare_and_swap_zero_expected_with_existing_zero_value() {
        let hash = random::<Hash>();
        let value = random::<Hash>();
        let expected = Hash::default(); // zero — "no previous value"

        let repository = random::<Context>();

        let mut dynamodb_mock = MockDynamoDb::default();

        let key_type = KeyType::BranchId;
        let typed_hash = AwsMutableStore::typed_key(hash, key_type);

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value))
                .unwrap();

        // DynamoDB succeeds: `attribute_not_exists(#v) OR #v = :expected(0)` matches
        // a row whose value attribute is zero.
        dynamodb_mock
            .expect_put_item_conditional()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(item.clone()),
                eq(CompareAndSwapCondition::new(expected)
                    .expect("failed to create CompareAndSwap condition")),
            )
            .return_once(move |_, _, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        let store = initialize_mutable_store(dynamodb_mock).await;
        let store = Arc::new(store);

        // The swap must be reported as successful (returning the expected zero hash) and
        // the new value must have been written — i.e. no false-positive no-op.
        assert_eq!(
            expected,
            store
                .compare_and_swap(repository.into(), hash, expected, value, key_type)
                .await
                .expect("should not have returned an error")
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

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(MutableStoreEntry::new(repository, typed_hash).with_value(value))
                .unwrap();

        dynamodb_mock
            .expect_put_item_conditional()
            .with(
                eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                eq(item.clone()),
                eq(CompareAndSwapCondition::new(expected)
                    .expect("failed to create CompareAndSwap condition")),
            )
            .return_once(move |_, _, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        let store = initialize_mutable_store(dynamodb_mock).await;
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
}
