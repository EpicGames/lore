// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use lore_base::error::AddressNotFound;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_storage::FlushOptions;
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

use crate::aws::error::AwsCallError;
use crate::aws::error::ErrorPayload;
use crate::aws::item::AttributeValue;
use crate::aws::item::Item;
use crate::aws::item::from_item;
use crate::aws::item::to_attribute_value;
use crate::aws::item::to_item;
use crate::aws_error::AwsError;
use crate::default_aws_timeout_millis;
use crate::dynamodb::ConditionParts;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbCondition;
use crate::dynamodb::DynamoDbQuery;
use crate::dynamodb::DynamoError;
use crate::dynamodb::ExprNames;
use crate::dynamodb::ExprValues;
use crate::dynamodb::ensure_cursor_advanced;
use crate::store::is_store_overloaded;
use crate::store::store_failure;
use crate::store::with_backoff;

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

#[lore_macro::test_pub]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MutableStoreEntry {
    repository_id: Context,
    key: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Hash>,
}

impl MutableStoreEntry {
    #[lore_macro::test_pub]
    fn new(repository_id: Context, key: Hash) -> Self {
        Self {
            repository_id,
            key,
            value: None,
        }
    }

    #[lore_macro::test_pub]
    fn with_value(mut self, value: Hash) -> Self {
        self.value = Some(value);
        self
    }

    /// This entry as a `DynamoDB` item.
    fn to_item(&self) -> Result<Item, StoreError> {
        to_item(self).map_err(|e| {
            warn!(entry = ?self, error = ?e, "Failed to convert mutable store entry to a DynamoDB item");
            StoreError::internal_with_context(
                e,
                "failed to convert mutable store entry to dynamo attribute value map",
            )
        })
    }
}

#[lore_macro::test_pub]
#[derive(Debug, PartialEq)]
struct CompareAndSwapCondition {
    condition_expression: &'static str,
    expression_names: ExprNames,
    expression_values: ExprValues,
}

impl CompareAndSwapCondition {
    #[lore_macro::test_pub]
    fn new(expected: Hash) -> Result<Self, StoreError> {
        const VALUE_ATTRIBUTE_ALIAS: &str = "#v";
        const VALUE_PLACEHOLDER: &str = ":value";

        let mut expression_names = ExprNames::new();
        let mut expression_values = ExprValues::new();

        let expected_value = to_attribute_value(expected).map_err(|e| {
            warn!("Failed to convert expected value: {expected:?} to dynamo attribute value map: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize expected hash for DynamoDB condition")
        })?;

        expression_names.push((VALUE_ATTRIBUTE_ALIAS, MUTABLE_STORE_VALUE_ATTRIBUTE));
        expression_values.push((VALUE_PLACEHOLDER, expected_value));

        // A zero `expected` treats an absent value and a stored zero as the same starting state.
        let condition_expression = if expected.is_zero() {
            "attribute_not_exists(#v) OR #v = :value"
        } else {
            "#v = :value"
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

#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct ListQuery {
    repository: Context,
    key_type: KeyType,
}

impl DynamoDbQuery for ListQuery {
    fn key_condition_expression(&self) -> &str {
        "#repo = :repo AND #key BETWEEN :ks AND :ke"
    }

    fn expression_attribute_names(&self) -> ExprNames {
        ExprNames::from_slice(&[("#repo", "repository_id"), ("#key", "key")])
    }

    fn expression_attribute_values(&self) -> ExprValues {
        // Construct the start and end values to include all entries of a given type
        let mut key_start = [0u8; 32];
        key_start[0] = self.key_type as u8;
        let mut key_end = [0xFFu8; 32];
        key_end[0] = self.key_type as u8;

        smallvec::smallvec![
            (":repo", AttributeValue::blob(self.repository.as_bytes())),
            (":ks", AttributeValue::blob(key_start)),
            (":ke", AttributeValue::blob(key_end)),
        ]
    }

    fn consistent_read(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct AwsMutableStoreSettings {
    pub dynamodb: DynamoDbMutableStoreSettings,
}

impl AwsMutableStoreSettings {
    pub fn new(dynamodb: DynamoDbMutableStoreSettings) -> Self {
        Self { dynamodb }
    }
}

pub struct AwsMutableStore {
    dynamodb: DynamoDb,
    mutable_store_table_name: Arc<str>,
}

impl AwsMutableStore {
    pub fn new(dynamodb: DynamoDb, settings: &AwsMutableStoreSettings) -> Self {
        info!("Creating AWS mutable store");
        Self {
            dynamodb,
            mutable_store_table_name: Arc::from(settings.dynamodb.mutable_store_table_name.clone()),
        }
    }

    #[lore_macro::test_pub]
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

        let output = self
            .dynamodb
            .get_item(
                &self.mutable_store_table_name,
                key_item,
                true, /* consistent read */
            )
            .await
            .map_err(|err| store_failure(err, "failed to read a mutable store entry"))?;

        let not_found = || StoreError::from(AddressNotFound::from(Address::zero_context_hash(key)));
        let Some(av_map) = output.item else {
            return Err(not_found());
        };

        let item: MutableStoreEntry = from_item(av_map).map_err(|e| {
            warn!("Failed to deserialize dynamodb item into mutable store entry: {e:?}");
            StoreError::internal_with_context(
                e,
                "failed to deserialize dynamodb item into mutable store entry",
            )
        })?;

        // A stored zero reads as absent, as it does in the local store.
        item.value
            .filter(|value| !value.is_zero())
            .ok_or_else(not_found)
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
                    store_failure(
                        err,
                        "failed to delete a mutable store entry while storing an empty value",
                    )
                })
        } else {
            let item = entry.with_value(value).to_item()?;
            self.dynamodb
                .put_item(&self.mutable_store_table_name, item)
                .await
                .map(|_| ())
                .map_err(|err| store_failure(err, "failed to write a mutable store entry"))
        }
    }

    /// Swaps `key` from `expected` to `value` as [`MutableStoreTrait::compare_and_swap`] describes:
    /// by a put on the swap's condition, by a delete on it for a zero `value`, and by a read for
    /// a `value` equal to a non-zero `expected`, which has nothing to write.
    ///
    /// A put refused against an item that already holds `value` answers `expected`, as a swap that
    /// applied does. A resend after a lost answer finds this call's own write there, and an item
    /// another writer set to the same value is the state this call asked for. A delete leaves no
    /// item to tell its own resend by, so a refused one answers what the refusal found, zero for no
    /// item, as a swap to zero of an absent key does.
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
                    Err(err) => match condition_refusal(err) {
                        Ok(refused) => held_on_refusal(repository, key, expected, refused),
                        Err(err) => Err(store_failure(
                            err,
                            "DynamoDB conditional delete failed for a mutable store compare-and-swap",
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
                Err(err) => match condition_refusal(err) {
                    Ok(refused) => match held_on_refusal(repository, key, expected, refused) {
                        Ok(held) if held == value => Ok(expected),
                        held => held,
                    },
                    Err(err) => Err(store_failure(
                        err,
                        "DynamoDB conditional put failed for a mutable store compare-and-swap",
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
        let store = self.clone();

        lore_base::lore_spawn!(
            async move {
                let mut last_evaluated_key = None;
                let mut pages = 0u64;
                'pages: loop {
                    pages += 1;

                    // A ladder per page rather than one for the whole listing: a listing is as many
                    // requests as it has pages, and a budget shared across them would spend itself
                    // on the first congested page and then cut the rest of the stream short.
                    let mut backoff = lore_storage::store_retry();

                    let output = loop {
                        match store
                            .dynamodb
                            .query_page(&table_name, query, &last_evaluated_key)
                            .await
                        {
                            Ok(output) => break output,
                            Err(err) if is_store_overloaded(&err) && backoff.wait().await => {
                                debug!(
                                    {REPOSITORY_ID} = %repository,
                                    error = ?err,
                                    "A mutable store listing page was shed, backing off"
                                );
                            }
                            Err(err) => {
                                // The stream carries values and nothing else, so a consumer cannot
                                // tell a listing that ended from one that gave up. That makes this
                                // the only record that the answer was incomplete.
                                warn!(
                                    {REPOSITORY_ID} = %repository,
                                    error = ?err,
                                    "Mutable store listing gave up part way; the stream ends short \
                                     of the full answer"
                                );
                                break 'pages;
                            }
                        }
                    };

                    if let Some(items) = output.items {
                        for item in items {
                            if let Ok(entry) = from_item::<MutableStoreEntry>(item).map_err(|e| {
                                warn!(error = ?e, "Error converting DDB item to MutableStoreEntry");
                            }) && let Some(value) = entry.value
                                && !value.is_zero()
                                && let Err(err) = sender.send((entry.key, value))
                            {
                                debug!(err = %err, "Failed sending mutable list result");
                                return;
                            }
                        }
                    }

                    let Some(key) = output.last_evaluated_key.filter(|key| !key.is_empty()) else {
                        break 'pages;
                    };
                    if ensure_cursor_advanced(&table_name, pages, last_evaluated_key.as_ref(), &key)
                        .is_err()
                    {
                        break 'pages;
                    }
                    last_evaluated_key = Some(key);
                }
            }
            .in_current_span()
        );

        Ok(stream)
    }
}

/// The item `DynamoDB` returned with its refusal of a write on the write's condition, `None` for a
/// refusal that returned no item, or `err` back for any other failure.
fn condition_refusal(err: DynamoError) -> Result<Option<Item>, DynamoError> {
    match err {
        AwsError::Aws(AwsCallError::Service(service))
            if service.code.is_conditional_check_failed_exception() =>
        {
            Ok(match service.payload {
                ErrorPayload::Item(item) => Some(*item),
                _ => None,
            })
        }
        err => Err(err),
    }
}

/// The value a swap of `key` in `repository` from `expected` answers when `DynamoDB` refused its
/// write and returned `item`, the item as the swap's condition found it: the value the item holds,
/// or zero for no item or an item with no value.
///
/// # Errors
///
/// An internal error when the item cannot be read, or when it holds `expected`, which the
/// condition matches: answered, `expected` would read as a swap made.
fn held_on_refusal(
    repository: Context,
    key: Hash,
    expected: Hash,
    item: Option<Item>,
) -> Result<Hash, StoreError> {
    let held = match item {
        Some(item) => {
            let entry: MutableStoreEntry = from_item(item).map_err(|e| {
                warn!(
                    {REPOSITORY_ID} = %repository,
                    %key,
                    error = %e,
                    "Failed to parse the item a compare-and-swap was refused against"
                );
                StoreError::internal_with_context(
                    e,
                    "failed to convert mutable store value from item",
                )
            })?;
            entry.value.unwrap_or_default()
        }
        None => Hash::default(),
    };
    if held == expected {
        warn!(
            {REPOSITORY_ID} = %repository,
            %key,
            %expected,
            "DynamoDB refused a compare and swap from the value the key holds"
        );
        return Err(StoreError::internal(
            "DynamoDB refused a compare and swap from the value the key holds",
        ));
    }
    Ok(held)
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
            {
                with_backoff(StoreError::is_slow_down, || {
                    self.clone().load_typed(repository, typed_key)
                })
                .await
            }
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
            {
                with_backoff(StoreError::is_slow_down, || {
                    self.clone().store_typed(repository, typed_key, value)
                })
                .await
            }
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
                with_backoff(StoreError::is_slow_down, || {
                    self.clone()
                        .compare_and_swap_typed(repository, typed_key, expected, value)
                })
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
