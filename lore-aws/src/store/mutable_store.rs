// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::error::ConditionalCheckFailedException;
use aws_smithy_types::Blob;
use lore_base::error::AddressNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_storage::FlushOptions;
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
use tracing::Level;
use tracing::debug;
use tracing::info;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::aws_error::AwsError;
use crate::default_aws_timeout_millis;
use crate::dynamodb::ConditionParts;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbCondition;
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

    /// This entry as a `DynamoDB` item.
    fn to_item(&self) -> Result<HashMap<String, AttributeValue>, StoreError> {
        serde_dynamo::to_item(self).map_err(|e| {
            warn!(entry = ?self, error = ?e, "Failed to convert mutable store entry to a DynamoDB item");
            StoreError::internal_with_context(
                e,
                "failed to convert mutable store entry to dynamo attribute value map",
            )
        })
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

        // A key holds zero both with no item and with an item holding zero, so a swap from zero
        // matches either, where an item-existence condition would refuse the second.
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

impl DynamoDbCondition for CompareAndSwapCondition {
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
        let key_item = MutableStoreEntry::new(repository, key).to_item()?;

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
            Err(err) => Err(request_failed(
                err,
                repository,
                key,
                "DynamoDB read of a mutable key failed",
                Level::WARN,
            )),
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
            let item = entry.to_item()?;

            self.dynamodb
                .delete_item(&self.mutable_store_table_name, item)
                .await
                .map(|_| ())
                .map_err(|err| {
                    request_failed(
                        err,
                        repository,
                        key,
                        "DynamoDB delete of a mutable key stored as zero failed",
                        Level::WARN,
                    )
                })
        } else {
            let item = entry.with_value(value).to_item()?;
            self.dynamodb
                .put_item(&self.mutable_store_table_name, item)
                .await
                .map(|_| ())
                .map_err(|err| {
                    request_failed(
                        err,
                        repository,
                        key,
                        "DynamoDB put of a mutable key failed",
                        Level::DEBUG,
                    )
                })
        }
    }

    /// Swaps `key` from `expected` to `value` as [`MutableStoreTrait::compare_and_swap`] describes:
    /// by a put on the swap's condition, by a delete on it for a zero `value`, and by a read for
    /// a `value` equal to a non-zero `expected`, which has nothing to write.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments. The delete is written
    /// out here for the same reason: a function of its own would hold another.
    #[allow(clippy::manual_async_fn)]
    fn compare_and_swap_typed(
        self: Arc<Self>,
        repository: Context,
        key: Hash,
        expected: Hash,
        value: Hash,
    ) -> impl Future<Output = Result<Hash, StoreError>> {
        async move {
            if value.is_zero() {
                let item = MutableStoreEntry::new(repository, key).to_item()?;
                let condition = CompareAndSwapCondition::new(expected)?;
                let result = self
                    .dynamodb
                    .delete_item_conditional(&self.mutable_store_table_name, item, condition)
                    .await;
                return match result {
                    Ok(_) => Ok(expected),
                    Err(err) => match condition_refusal(&err, |e| match e {
                        DeleteItemError::ConditionalCheckFailedException(refusal) => Some(refusal),
                        _ => None,
                    }) {
                        Some(refusal) => held_on_refusal(expected, refusal.item()),
                        None => Err(request_failed(
                            err,
                            repository,
                            key,
                            "DynamoDB write of a mutable key's compare and swap failed",
                            Level::WARN,
                        )),
                    },
                };
            }
            if value == expected {
                return match self.load_typed(repository, key).await {
                    Err(err) if err.is_address_not_found() => Ok(Hash::default()),
                    held => held,
                };
            }

            let item = MutableStoreEntry::new(repository, key)
                .with_value(value)
                .to_item()?;
            let condition = CompareAndSwapCondition::new(expected)?;
            let result = self
                .dynamodb
                .put_item_conditional(&self.mutable_store_table_name, item, condition)
                .await;
            match result {
                Ok(_) => Ok(expected),
                Err(err) => match condition_refusal(&err, |e| match e {
                    PutItemError::ConditionalCheckFailedException(refusal) => Some(refusal),
                    _ => None,
                }) {
                    Some(refusal) => held_on_refusal(expected, refusal.item()),
                    None => Err(request_failed(
                        err,
                        repository,
                        key,
                        "DynamoDB write of a mutable key's compare and swap failed",
                        Level::WARN,
                    )),
                },
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

/// The refusal `err` reports when `DynamoDB` refused a write on its condition, or `None` for any
/// other failure. `pick` takes the refusal out of the operation's error.
fn condition_refusal<E>(
    err: &AwsError<SdkError<E>>,
    pick: impl FnOnce(&E) -> Option<&ConditionalCheckFailedException>,
) -> Option<&ConditionalCheckFailedException> {
    match err {
        AwsError::AwsSdkError(sdk_error) => sdk_error.as_service_error().and_then(pick),
        _ => None,
    }
}

/// The value a swap from `expected` answers when `DynamoDB` refused its write and returned `item`,
/// the item as the swap's condition found it: the value the item holds, or zero for no item or an
/// item with no value.
///
/// # Errors
///
/// An internal error when the item cannot be read, or when it holds `expected`, which the
/// condition matches: answered, `expected` would read as a swap made.
fn held_on_refusal(
    expected: Hash,
    item: Option<&HashMap<String, AttributeValue>>,
) -> Result<Hash, StoreError> {
    let held = match item {
        Some(item) => {
            let entry: MutableStoreEntry =
                serde_dynamo::from_item(item.to_owned()).map_err(|e| {
                    warn!(?item, error = %e, "Failed to parse mutable store value from item");
                    StoreError::internal_with_context(
                        e,
                        "Failed to convert mutable store value from item",
                    )
                })?;
            entry.value.unwrap_or_default()
        }
        None => Hash::default(),
    };
    if held == expected {
        warn!(%expected, "DynamoDB refused a compare and swap from the value the key holds");
        return Err(StoreError::internal(
            "DynamoDB refused a compare and swap from the value the key holds",
        ));
    }
    Ok(held)
}

/// The error a `DynamoDB` request for `key` in `repository` fails with, other than on a write's
/// condition: a slow-down for an error the SDK reports, and an internal error named `what` for any
/// other. Logs `what` with the error, at debug for [`Level::DEBUG`] and at warn otherwise.
#[track_caller]
fn request_failed<E>(
    err: AwsError<E>,
    repository: Context,
    key: Hash,
    what: &'static str,
    level: Level,
) -> StoreError
where
    AwsError<E>: std::error::Error + Send + Sync + 'static,
{
    if level == Level::DEBUG {
        debug!({REPOSITORY_ID} = %repository, key = %key, error = ?err, "{what}");
    } else {
        warn!({REPOSITORY_ID} = %repository, key = %key, error = ?err, "{what}");
    }
    match err {
        AwsError::AwsSdkError(_sdk_error) => StoreError::from(SlowDown),
        _ => StoreError::internal_with_context(err, what),
    }
}

#[async_trait]
impl MutableStoreTrait for AwsMutableStore {
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

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

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
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

    use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
    use aws_sdk_dynamodb::operation::delete_item::DeleteItemOutput;
    use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
    use aws_sdk_dynamodb::operation::put_item::PutItemError;
    use aws_sdk_dynamodb::operation::put_item::PutItemOutput;
    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_sdk_dynamodb::types::error::ConditionalCheckFailedException;
    use aws_sdk_dynamodb::types::error::ProvisionedThroughputExceededException;
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

    /// A swap from zero of a key whose item holds zero puts the new value and answers zero: the
    /// swap's condition matches an item holding zero as it matches no item.
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

    const KEY_TYPE: KeyType = KeyType::BranchLatestPointer;

    /// The item key of `hash` in `repository`, with no value.
    fn item_key(repository: Context, hash: Hash) -> HashMap<String, AttributeValue> {
        serde_dynamo::to_item(MutableStoreEntry::new(
            repository,
            AwsMutableStore::typed_key(hash, KEY_TYPE),
        ))
        .unwrap()
    }

    /// The item of `hash` in `repository` holding `value`.
    fn item(repository: Context, hash: Hash, value: Hash) -> HashMap<String, AttributeValue> {
        serde_dynamo::to_item(
            MutableStoreEntry::new(repository, AwsMutableStore::typed_key(hash, KEY_TYPE))
                .with_value(value),
        )
        .unwrap()
    }

    /// The service error `error`, as the SDK reports one.
    fn refused<E>(error: E) -> AwsError<SdkError<E, HttpResponse>> {
        AwsError::sdk_error(SdkError::ServiceError(
            aws_smithy_runtime_api::client::result::ServiceError::builder()
                .source(error)
                .raw(HttpResponse::new(
                    400u16.try_into().unwrap(),
                    SdkBody::empty(),
                ))
                .build(),
        ))
    }

    /// A put refused on its condition, returning `item`.
    fn refused_put(
        item: Option<HashMap<String, AttributeValue>>,
    ) -> AwsError<SdkError<PutItemError, HttpResponse>> {
        refused(PutItemError::ConditionalCheckFailedException(
            ConditionalCheckFailedException::builder()
                .set_item(item)
                .build(),
        ))
    }

    /// A delete refused on its condition, returning `item`.
    fn refused_delete(
        item: Option<HashMap<String, AttributeValue>>,
    ) -> AwsError<SdkError<DeleteItemError, HttpResponse>> {
        refused(DeleteItemError::ConditionalCheckFailedException(
            ConditionalCheckFailedException::builder()
                .set_item(item)
                .build(),
        ))
    }

    async fn swap(
        dynamodb: DynamoDb,
        repository: Context,
        hash: Hash,
        expected: Hash,
        value: Hash,
    ) -> Result<Hash, lore_storage::StoreError> {
        Arc::new(initialize_mutable_store(dynamodb).await)
            .compare_and_swap(repository.into(), hash, expected, value, KEY_TYPE)
            .await
    }

    /// A swap to zero deletes the item on the swap's condition, whether it swaps from a value or
    /// from zero, and answers `expected` when the delete is made.
    #[tokio::test]
    async fn a_swap_to_zero_deletes_the_item_on_its_condition() {
        for expected in [random::<Hash>(), Hash::default()] {
            let (hash, repository) = (random::<Hash>(), random::<Context>());
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_delete_item_conditional()
                .with(
                    eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                    eq(item_key(repository, hash)),
                    eq(CompareAndSwapCondition::new(expected).unwrap()),
                )
                .times(1)
                .return_once(|_, _, _| Ok(DeleteItemOutput::builder().build()));

            assert_eq!(
                swap(dynamodb, repository, hash, expected, Hash::default())
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
        let returned = item(repository, hash, held);
        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_delete_item_conditional::<CompareAndSwapCondition>()
            .return_once(move |_, _, _| Err(refused_delete(Some(returned))));

        assert_eq!(
            swap(dynamodb, repository, hash, expected, Hash::default())
                .await
                .expect("swaps"),
            held
        );
    }

    /// A refused swap answers zero when `DynamoDB` returns no item with the refusal, or an item
    /// with no value, by a put and by a delete alike.
    #[tokio::test]
    async fn a_refused_swap_answers_zero_for_no_item_or_an_item_without_a_value() {
        for to_zero in [false, true] {
            for valueless in [false, true] {
                let (hash, repository) = (random::<Hash>(), random::<Context>());
                let returned = valueless.then(|| item_key(repository, hash));
                let mut dynamodb = MockDynamoDb::default();
                if to_zero {
                    dynamodb
                        .expect_delete_item_conditional::<CompareAndSwapCondition>()
                        .return_once(move |_, _, _| Err(refused_delete(returned)));
                } else {
                    dynamodb
                        .expect_put_item_conditional::<CompareAndSwapCondition>()
                        .return_once(move |_, _, _| Err(refused_put(returned)));
                }
                let value = if to_zero {
                    Hash::default()
                } else {
                    random::<Hash>()
                };

                assert_eq!(
                    swap(dynamodb, repository, hash, random::<Hash>(), value)
                        .await
                        .expect("swaps"),
                    Hash::default(),
                    "to zero {to_zero}, an item without a value {valueless}"
                );
            }
        }
    }

    /// A refusal whose returned item meets the swap's condition contradicts it, so the swap fails
    /// rather than answer `expected`, which would read as made: by a put and by a delete, from a
    /// value or from zero with no item returned.
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
                let returned = (!from_zero).then(|| item(repository, hash, expected));
                let mut dynamodb = MockDynamoDb::default();
                let value = if to_zero {
                    dynamodb
                        .expect_delete_item_conditional::<CompareAndSwapCondition>()
                        .return_once(move |_, _, _| Err(refused_delete(returned)));
                    Hash::default()
                } else {
                    dynamodb
                        .expect_put_item_conditional::<CompareAndSwapCondition>()
                        .return_once(move |_, _, _| Err(refused_put(returned)));
                    random::<Hash>()
                };

                let swapped = swap(dynamodb, repository, hash, expected, value).await;
                assert!(
                    swapped.is_err(),
                    "to zero {to_zero}, from zero {from_zero}: answered {swapped:?}"
                );
            }
        }
    }

    /// A delete that fails other than on its condition fails the swap as a put does: an SDK error
    /// as a slow-down, any other as an internal error.
    #[tokio::test]
    async fn a_failed_delete_fails_the_swap() {
        for sdk in [true, false] {
            let (hash, repository) = (random::<Hash>(), random::<Context>());
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_delete_item_conditional::<CompareAndSwapCondition>()
                .return_once(move |_, _, _| {
                    Err(if sdk {
                        refused(DeleteItemError::ProvisionedThroughputExceededException(
                            ProvisionedThroughputExceededException::builder().build(),
                        ))
                    } else {
                        AwsError::JoinError
                    })
                });

            let err = swap(
                dynamodb,
                repository,
                hash,
                random::<Hash>(),
                Hash::default(),
            )
            .await
            .expect_err("the swap fails");
            assert_eq!(err.is_slow_down(), sdk, "an SDK error {sdk}: {err:?}");
        }
    }

    /// A swap from the held value to itself reads the item and writes nothing, answering what the
    /// item holds: the value, another value, or zero for no item.
    #[tokio::test]
    async fn a_swap_to_the_expected_value_reads_and_writes_nothing() {
        let value = random::<Hash>();
        for held in [Some(value), Some(random::<Hash>()), None] {
            let (hash, repository) = (random::<Hash>(), random::<Context>());
            let returned = held.map(|held| item(repository, hash, held));
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_get_item()
                .with(
                    eq(Arc::<str>::from(MUTABLE_STORE_TABLE_NAME)),
                    eq(item_key(repository, hash)),
                    eq(true),
                )
                .times(1)
                .return_once(move |_, _, _| {
                    Ok(GetItemOutput::builder().set_item(returned).build())
                });

            assert_eq!(
                swap(dynamodb, repository, hash, value, value)
                    .await
                    .expect("swaps"),
                held.unwrap_or_default()
            );
        }
    }
}
