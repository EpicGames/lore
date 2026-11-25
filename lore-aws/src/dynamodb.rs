// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use lore_base::retry::Retry;
use lore_revision::util::time::RetryPolicy;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::drop_record::DropRecord;
use lore_telemetry::drop_time::DropTimeMs;
use lore_telemetry::observe::Observe;
#[cfg(feature = "test-util")]
use mockall::mock;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Histogram;
use tokio::task::JoinSet;
use tracing::Instrument;
use uuid::Uuid;

use crate::aws::dynamodb::BatchGetItemOutput;
use crate::aws::dynamodb::Condition;
use crate::aws::dynamodb::DynamoClient;
use crate::aws::dynamodb::DynamoResult;
pub use crate::aws::dynamodb::ExprNames;
pub use crate::aws::dynamodb::ExprValues;
use crate::aws::dynamodb::GetItemOutput;
use crate::aws::dynamodb::QueryOutput;
use crate::aws::dynamodb::QueryRequest;
pub use crate::aws::dynamodb::ReturnValue;
use crate::aws::dynamodb::ScanRequest;
pub use crate::aws::dynamodb::Select;
pub use crate::aws::dynamodb::TransactDelete;
pub use crate::aws::dynamodb::TransactPut;
pub use crate::aws::dynamodb::TransactWriteItem;
use crate::aws::dynamodb::WriteOutput;
use crate::aws::error::AwsCallError;
use crate::aws::error::DynamoErrorCode;
use crate::aws::item::Item;
use crate::aws::observe::observe_aws_operation_callback;
use crate::aws_error::AwsError;
use crate::dynamodb::query_output_accumulation::QueryOutputAccumulation;

pub mod cancellation_reason;
pub mod query_output_accumulation;

/// What a call to this facade can fail with.
///
/// One alias for every operation: the error code is a typed enum, so the operation it came from is
/// not part of the type.
pub type DynamoError = AwsError<AwsCallError<DynamoErrorCode>>;

pub const METRICS_TABLE_NAME_KEY: &str = "table_name";
const METRICS_PUT_ITEM_CONTEXT: &str = "put_item";
const METRICS_DELETE_ITEM_CONTEXT: &str = "delete_item";
const METRICS_CONDITIONAL_ATTRIBUTE: &str = "conditional";

const METRICS_BATCH_OPERATION_TOTAL_SIZE: &str = "batch_operation_size";
const SIZE_BOUNDARIES: [f64; 15] = [
    1., 5., 10., 50., 100., 200., 300., 500., 2_500., 5_000., 10_000., 20_000., 40_000., 60_000.,
    80_000.,
];

const METRICS_NESTED_API_CALLS: &str = "nested_api_calls.total";
const NESTED_CALL_VOLUMES: [f64; 11] = [1., 2., 3., 6., 10., 20., 40., 80., 100., 150., 200.];

const METRICS_UNPROCESSED_RETRIES: &str = "unprocessed_retries.total";
const UNPROCESSED_RETRIES_BOUNDARIES: [f64; 11] =
    [0., 1., 2., 4., 7., 12., 18., 25., 30., 50., 100.];
const METRICS_UNPROCESSED_EXHAUSTED: &str = "unprocessed_retries.exhausted";

const DEFAULT_UNPROCESSED_KEYS_BACKOFF_START_MS: u64 = 50;
const DEFAULT_UNPROCESSED_KEYS_BACKOFF_MAX_MS: u64 = 1_000;
const DEFAULT_UNPROCESSED_KEYS_MAX_RESENDS: usize = 6;

/// The default backoff for resending one `BatchGetItem` chunk's unprocessed keys. Its limit counts
/// resends.
pub fn default_unprocessed_keys_retry() -> RetryPolicy {
    RetryPolicy::builder()
        .with_initial_backoff_millis(DEFAULT_UNPROCESSED_KEYS_BACKOFF_START_MS)
        .with_max_backoff_millis(DEFAULT_UNPROCESSED_KEYS_BACKOFF_MAX_MS)
        .with_limit(DEFAULT_UNPROCESSED_KEYS_MAX_RESENDS)
        .build()
}

const METRICS_QUERY_COUNT: &str = "query.accumulation.count";
const METRICS_QUERY_SCANNED_ROWS: &str = "query.accumulation.scanned_count";
const QUERY_ROWS: [f64; 10] = [
    10., 50., 100., 400., 1_200., 2_400., 7_200., 21_600., 64_800., 194_400.,
];

pub const METRICS_ACCUMULATION_OPERATION_LATENCY_METRIC_NAME: &str =
    "accumulation_operation_duration";

/// Temporary environment variable to allow enabling/disabling query pagination.
/// TODO(jcohen): Remove this when we're confident in the pagination support.
static ENABLE_QUERY_PAGINATION: LazyLock<bool> =
    LazyLock::new(|| match std::env::var("LORE_ENABLE_QUERY_PAGINATION") {
        Ok(v) => v.eq_ignore_ascii_case("true"),
        Err(_) => true, // Default to enabled if the env var isn't present
    });

/// A query this facade can run.
///
/// The two expression maps are ordered small vectors of `&'static str` keys rather than
/// `HashMap<String, _>`: every placeholder and attribute name in every implementation is a literal
/// or a `const`, so nothing here needs to own a `String` or hash it.
pub trait DynamoDbQuery {
    fn index_name(&self) -> Option<&'static str> {
        None
    }

    fn key_condition_expression(&self) -> &str;

    fn expression_attribute_names(&self) -> ExprNames;

    fn expression_attribute_values(&self) -> ExprValues;

    fn filter_expression(&self) -> Option<&'static str> {
        None
    }

    fn limit(&self) -> Option<i32> {
        None
    }

    fn select(&self) -> Option<Select> {
        None
    }

    fn consistent_read(&self) -> bool {
        false
    }
}

pub struct ConditionParts {
    pub condition_expression: &'static str,
    pub expression_names: ExprNames,
    pub expression_values: ExprValues,
}

#[derive(Default)]
pub struct ScanConfig {
    pub segment: Option<i32>,
    pub total_segments: Option<i32>,
    pub limit: Option<i32>,
}

impl ScanConfig {
    /// Create a segmented scan config for parallel scanning.
    pub fn segment(segment: i32, total_segments: i32) -> Self {
        Self {
            segment: Some(segment),
            total_segments: Some(total_segments),
            limit: None,
        }
    }
}

/// One page of a table scan: the items in the page paired with the `last_evaluated_key` to resume
/// from, or `None` once the end of the table has been reached.
pub struct ScanPage {
    pub items: Vec<Item>,
    pub last_evaluated_key: Option<Item>,
}

/// Encapsulate the components of a `DynamoDB` conditional put or delete. See the `DynamoDB` docs
/// for details.
/// <https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Expressions.ConditionExpressions.html>
pub trait DynamoDbCondition {
    /// Consume the condition to create its intrinsic parts
    fn into_parts(self) -> ConditionParts;
}

// This is the maximum number of items allowed in a single BatchGetItem call, no need to make this
// configurable for now.
pub const BATCH_GET_ITEM_MAX_COUNT: usize = 100;

// This is the maximum number of items allowed in a single TransactWriteItems call, no need to make
// this configurable for now.
pub const TRANSACT_WRITE_ITEMS_CHUNKS: usize = 100;

#[derive(Clone)]
struct DynamoDbInstrumentProvider {
    metrics_attributes: Vec<KeyValue>,
}

impl InstrumentProvider for DynamoDbInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.aws.dynamodb"
    }

    fn labels(&self) -> &[KeyValue] {
        &self.metrics_attributes
    }
}

#[derive(Clone)]
struct DynamoDbInstruments {
    provider: DynamoDbInstrumentProvider,
    batch_size_histogram: Histogram<u64>,
    accumulation_operation_latency_histogram: Histogram<f64>,
    operation_latency_histogram: Histogram<f64>,
    num_nested_api_calls_histogram: Histogram<u64>,
    unprocessed_retries_histogram: Histogram<u64>,
    unprocessed_exhausted_counter: Counter<u64>,
    query_accumulation_count_histogram: Histogram<u64>,
    query_accumulation_scanned_rows_histogram: Histogram<u64>,
}

impl InstrumentProvider for DynamoDbInstruments {
    fn namespace(&self) -> &'static str {
        self.provider.namespace()
    }

    fn labels(&self) -> &[KeyValue] {
        self.provider.labels()
    }
}

#[derive(Clone)]
pub struct DynamoDbImpl {
    client: DynamoClient,
    slow_operation_duration: Duration,
    /// The backoff for resending each `BatchGetItem` chunk's unprocessed keys.
    unprocessed_keys_retry: RetryPolicy,
    instruments: DynamoDbInstruments,
}

impl DynamoDbImpl {
    pub fn new(
        client: DynamoClient,
        slow_operation_duration: Duration,
        metrics_attributes: Vec<KeyValue>,
        unprocessed_keys_retry: RetryPolicy,
    ) -> Self {
        let instrument_provider = DynamoDbInstrumentProvider { metrics_attributes };

        Self {
            client,
            slow_operation_duration,
            unprocessed_keys_retry,

            instruments: DynamoDbInstruments {
                batch_size_histogram: instrument_provider
                    .length_histogram(METRICS_BATCH_OPERATION_TOTAL_SIZE, SIZE_BOUNDARIES.to_vec()),
                accumulation_operation_latency_histogram: instrument_provider
                    .latency_histogram_ms(METRICS_ACCUMULATION_OPERATION_LATENCY_METRIC_NAME),
                operation_latency_histogram: instrument_provider
                    .latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
                num_nested_api_calls_histogram: instrument_provider
                    .length_histogram(METRICS_NESTED_API_CALLS, NESTED_CALL_VOLUMES.to_vec()),
                unprocessed_retries_histogram: instrument_provider.length_histogram(
                    METRICS_UNPROCESSED_RETRIES,
                    UNPROCESSED_RETRIES_BOUNDARIES.to_vec(),
                ),
                unprocessed_exhausted_counter: instrument_provider
                    .counter(METRICS_UNPROCESSED_EXHAUSTED),
                query_accumulation_count_histogram: instrument_provider
                    .length_histogram(METRICS_QUERY_COUNT, QUERY_ROWS.to_vec()),
                query_accumulation_scanned_rows_histogram: instrument_provider
                    .length_histogram(METRICS_QUERY_SCANNED_ROWS, QUERY_ROWS.to_vec()),
                provider: instrument_provider,
            },
        }
    }

    #[tracing::instrument(name = "DynamoDbImpl::table_exists", skip_all)]
    pub async fn table_exists(&self, table_name: &Arc<str>) -> Result<bool, DynamoError> {
        let labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("table_exists");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.client
            .table_exists(table_name)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    #[tracing::instrument(name = "DynamoDbImpl::get_item", skip_all)]
    pub async fn get_item(
        &self,
        table_name: &Arc<str>,
        key: Item,
        consistent: bool,
    ) -> Result<GetItemOutput, DynamoError> {
        let labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("get_item");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.client
            .get_item(table_name, &key, consistent)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Whether `key` names an item, without decoding the item itself.
    ///
    /// Observed as `get_item`, which is the call it makes; the difference is only in how much of
    /// the answer is read.
    #[tracing::instrument(name = "DynamoDbImpl::item_exists", skip_all)]
    pub async fn item_exists(
        &self,
        table_name: &Arc<str>,
        key: Item,
        consistent: bool,
    ) -> Result<bool, DynamoError> {
        let labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("get_item");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.client
            .item_exists(table_name, &key, consistent)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Read `items` from `table_name`, in chunks of at most [`BATCH_GET_ITEM_MAX_COUNT`] keys read
    /// concurrently, resending the keys a chunk leaves unprocessed as [`batch_get_chunks`]
    /// describes. Items come back in no particular order.
    pub async fn batch_get_item(
        &self,
        table_name: &Arc<str>,
        items: Vec<Item>,
        consistent: bool,
    ) -> Result<Vec<Item>, DynamoError> {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("batch_get_item");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.instruments
            .batch_size_histogram
            .record(items.len() as u64, &base_labels);

        let _accumulation_time = DropTimeMs::new(
            self.instruments
                .accumulation_operation_latency_histogram
                .clone(),
            &base_labels,
        );
        let mut num_api_calls = DropRecord::new(
            self.instruments.num_nested_api_calls_histogram.clone(),
            &base_labels,
        );
        let mut unprocessed_retries = DropRecord::new(
            self.instruments.unprocessed_retries_histogram.clone(),
            &base_labels,
        );

        num_api_calls.set(items.len().div_ceil(BATCH_GET_ITEM_MAX_COUNT) as u64);

        let request = ObservedBatchGet {
            client: self.client.clone(),
            table: table_name.clone(),
            consistent,
            latency: self.instruments.operation_latency_histogram.clone(),
            labels: base_labels.clone(),
            slow_operation_duration: self.slow_operation_duration,
        };
        let read = batch_get_chunks(request, items, self.unprocessed_keys_retry).await;

        num_api_calls.add(read.resends);
        unprocessed_retries.add(read.resends);
        if matches!(read.items, Err(AwsError::UnprocessedKeys { .. })) {
            self.instruments
                .unprocessed_exhausted_counter
                .add(1, &base_labels);
        }

        read.items
    }

    /// The labels of a write of the operation `context` to `table_name`, conditional or not.
    fn write_labels(
        &self,
        context: &'static str,
        table_name: &Arc<str>,
        conditional: bool,
    ) -> LabelArray {
        let mut labels = self.instruments.get_labels_for_operation_context(context);
        labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
        labels.push(KeyValue::new(METRICS_CONDITIONAL_ATTRIBUTE, conditional));
        labels
    }

    #[tracing::instrument(name = "DynamoDbImpl::put_item", skip_all)]
    pub async fn put_item(
        &self,
        table_name: &Arc<str>,
        item: Item,
    ) -> Result<WriteOutput, DynamoError> {
        self.client
            .put_item(table_name, &item, None)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_PUT_ITEM_CONTEXT, table_name, false),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    #[tracing::instrument(name = "DynamoDbImpl::put_item_conditional", skip_all)]
    pub async fn put_item_conditional<T>(
        &self,
        table_name: &Arc<str>,
        item: Item,
        condition: T,
    ) -> Result<WriteOutput, DynamoError>
    where
        T: DynamoDbCondition + 'static,
    {
        let ConditionParts {
            condition_expression,
            expression_names,
            expression_values,
        } = condition.into_parts();

        // `ALL_OLD` on failure: the losing row is what identifies a lock's current holder.
        let condition = Condition {
            expression: condition_expression,
            names: &expression_names,
            values: &expression_values,
            return_values_on_failure: true,
        };

        self.client
            .put_item(table_name, &item, Some(&condition))
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_PUT_ITEM_CONTEXT, table_name, true),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    #[tracing::instrument(name = "DynamoDbImpl::delete_item", skip_all)]
    pub async fn delete_item(
        &self,
        table_name: &Arc<str>,
        key: Item,
    ) -> Result<WriteOutput, DynamoError> {
        self.client
            .delete_item(table_name, &key, None)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_DELETE_ITEM_CONTEXT, table_name, false),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Deletes the item at `key` when it meets `condition`. A refused condition fails with
    /// `ConditionalCheckFailedException` carrying the item as the condition found it, or no item
    /// when there was none.
    #[tracing::instrument(name = "DynamoDbImpl::delete_item_conditional", skip_all)]
    pub async fn delete_item_conditional<T>(
        &self,
        table_name: &Arc<str>,
        key: Item,
        condition: T,
    ) -> Result<WriteOutput, DynamoError>
    where
        T: DynamoDbCondition + 'static,
    {
        let ConditionParts {
            condition_expression,
            expression_names,
            expression_values,
        } = condition.into_parts();

        let condition = Condition {
            expression: condition_expression,
            names: &expression_names,
            values: &expression_values,
            return_values_on_failure: true,
        };

        self.client
            .delete_item(table_name, &key, Some(&condition))
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_DELETE_ITEM_CONTEXT, table_name, true),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    #[allow(clippy::doc_markdown)]
    /// This method supports transactions that exceed DynamoDB's intrinsic restriction of 100 items
    /// per transaction. In scenarios where a transaction is supplied that contains more than this
    /// limit, the items will be chunked up into batches of 100 items, meaning that it's possible
    /// to have one batch succeed and the next batch fail. In cases where atomicity is needed across
    /// batches larger than 100 items, it is up to the caller to ensure this requirement.
    ///
    /// Each chunk carries its own `ClientRequestToken`, [`chunk_token`] of `token`. A caller that
    /// sends the same items again under the same `token` therefore replays every chunk under the
    /// token it first went out with, and the service answers a chunk it already applied with that
    /// success rather than applying it twice — for a lock release, rather than finding the locks
    /// gone. The service holds a token for ten minutes and caps it at 36 characters, which a UUID
    /// fills.
    #[tracing::instrument(name = "DynamoDbImpl::transact_write_items", skip_all)]
    pub async fn transact_write_items(
        &self,
        table_name: &Arc<str>,
        items: Vec<TransactWriteItem>,
        token: Uuid,
    ) -> Result<Vec<WriteOutput>, DynamoError> {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("transact_write_items");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.instruments
            .batch_size_histogram
            .record(items.len() as u64, &base_labels);
        let _accumulation_time = DropTimeMs::new(
            self.instruments
                .accumulation_operation_latency_histogram
                .clone(),
            &base_labels,
        );
        let mut num_api_calls = DropRecord::new(
            self.instruments.num_nested_api_calls_histogram.clone(),
            &base_labels,
        );

        let mut join_set = JoinSet::new();
        let observe_fn = observe_aws_operation_callback(self.slow_operation_duration);
        let histogram = self.instruments.operation_latency_histogram.clone();

        for (index, chunk) in into_chunks(items, TRANSACT_WRITE_ITEMS_CHUNKS).enumerate() {
            num_api_calls.add(1);

            let client = self.client.clone();
            let labels = base_labels.clone();
            let histogram = histogram.clone();
            let token = chunk_token(token, index);

            lore_base::lore_spawn!(
                join_set,
                async move {
                    let mut encoded = [0u8; uuid::fmt::Hyphenated::LENGTH];
                    let token = token.hyphenated().encode_lower(&mut encoded);

                    client
                        .transact_write_items(&chunk, token)
                        .observe(histogram, labels, observe_fn)
                        .await
                }
                .in_current_span()
            );
        }

        let mut output = Vec::new();

        // Consume the results as they come in. Note: we do not maintain the order of incoming items
        // in the returned output.
        while let Some(join_result) = join_set.join_next().await {
            let result = join_result.map_err(|e| {
                tracing::warn!("Failed to join TransactWriteItems task: {e:?}");
                AwsError::JoinError
            })?;

            match result.output {
                Ok(r) => output.push(r),
                Err(e) => return Err(AwsError::Aws(e)),
            }
        }

        Ok(output)
    }

    /// Run a query, following its pages to the end.
    ///
    /// Sequential, since each page's request needs the previous page's `LastEvaluatedKey`. Records
    /// the nested call count, the accumulated and scanned rows, and each request's latency. A page
    /// answered with the cursor it resumed from fails the query with [`AwsError::StalledCursor`].
    #[tracing::instrument(name = "DynamoDbImpl::query", skip_all)]
    pub async fn query_paginated<T>(
        &self,
        table_name: &Arc<str>,
        query: T,
    ) -> Result<QueryOutputAccumulation, DynamoError>
    where
        T: DynamoDbQuery + 'static,
    {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("query_paginated");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        let _accumulation_time = DropTimeMs::new(
            self.instruments
                .accumulation_operation_latency_histogram
                .clone(),
            &base_labels,
        );
        let mut num_api_calls = DropRecord::new(
            self.instruments.num_nested_api_calls_histogram.clone(),
            &base_labels,
        );
        let mut query_count = DropRecord::new(
            self.instruments.query_accumulation_count_histogram.clone(),
            &base_labels,
        );
        let mut query_scanned_count = DropRecord::new(
            self.instruments
                .query_accumulation_scanned_rows_histogram
                .clone(),
            &base_labels,
        );

        let mut output = QueryOutputAccumulation::default();
        let histogram = self.instruments.operation_latency_histogram.clone();
        let observe_fn = observe_aws_operation_callback(self.slow_operation_duration);

        let names = query.expression_attribute_names();
        let values = query.expression_attribute_values();
        let mut exclusive_start_key: Option<Item> = None;
        let mut pages = 0u64;

        loop {
            num_api_calls.add(1);
            pages += 1;

            let request = query_request(
                table_name,
                &query,
                &names,
                &values,
                exclusive_start_key.as_ref(),
            );

            let page = self
                .client
                .query(&request)
                .observe(histogram.clone(), base_labels.clone(), observe_fn)
                .await
                .output
                .map_err(AwsError::Aws)?;

            query_count.add(page.count() as u64);
            query_scanned_count.add(page.scanned_count() as u64);

            let last_evaluated_key = page.last_evaluated_key;
            output.extend(page.items, page.count);

            // Perform a follow-up query if there are more pages to be retrieved. We want to
            // fetch more pages if:
            // 1. There is a last evaluated key on the response, and that key is *not* an
            //    empty AV map.
            // 2. However, if there was a limit set on the query and we've already fetched
            //    the desired number of rows, there's no need to continue fetching more
            //    rows.
            // 3. *Unless* the query is also a count query (in which case we need to fetch
            //    until there's no more data to collect to ensure we get the correct count).
            // TODO(jcohen): We should probably disallow the combination of `Select::Count`
            //  and a limit to avoid the degenerate case of fetching a full count one row at
            //  a time.
            let wanted = *ENABLE_QUERY_PAGINATION
                && (query.limit().is_none()
                    || matches!(query.select(), Some(Select::Count))
                    || query.limit().is_some_and(|l| output.count < l));

            match last_evaluated_key {
                Some(next) if wanted && !next.is_empty() => {
                    ensure_cursor_advanced(table_name, pages, exclusive_start_key.as_ref(), &next)?;
                    exclusive_start_key = Some(next);
                }
                _ => return Ok(output),
            }
        }
    }

    #[tracing::instrument(name = "DynamoDbImpl::query_single", skip_all)]
    pub async fn query_single<T>(
        &self,
        table_name: &Arc<str>,
        query: T,
    ) -> Result<QueryOutput, DynamoError>
    where
        T: DynamoDbQuery + 'static,
    {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("query_single");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        let mut query_count = DropRecord::new(
            self.instruments.query_accumulation_count_histogram.clone(),
            &base_labels,
        );
        let mut query_scanned_count = DropRecord::new(
            self.instruments
                .query_accumulation_scanned_rows_histogram
                .clone(),
            &base_labels,
        );

        let histogram = self.instruments.operation_latency_histogram.clone();
        let observe_fn = observe_aws_operation_callback(self.slow_operation_duration);

        let names = query.expression_attribute_names();
        let values = query.expression_attribute_values();
        let request = query_request(table_name, &query, &names, &values, None);

        match self
            .client
            .query(&request)
            .observe(histogram, base_labels.clone(), observe_fn)
            .await
            .output
        {
            Ok(r) => {
                query_count.add(r.count() as u64);
                query_scanned_count.add(r.scanned_count() as u64);

                Ok(r)
            }
            Err(e) => Err(AwsError::Aws(e)),
        }
    }

    /// Run one page of a query, resuming after `exclusive_start_key` when it names a row.
    ///
    /// For a caller that consumes rows as they arrive, where [`Self::query_paginated`] gathers
    /// every page before returning. The cursor is borrowed as `&Option<Item>` rather than
    /// `Option<&Item>`, because the mock generated under `test-util` cannot express a lifetime
    /// inside `Option` on a method generic over the query.
    #[tracing::instrument(name = "DynamoDbImpl::query_page", skip_all)]
    pub async fn query_page<T>(
        &self,
        table_name: &Arc<str>,
        query: T,
        exclusive_start_key: &Option<Item>,
    ) -> Result<QueryOutput, DynamoError>
    where
        T: DynamoDbQuery + 'static,
    {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("query_page");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        let names = query.expression_attribute_names();
        let values = query.expression_attribute_values();
        let request = query_request(
            table_name,
            &query,
            &names,
            &values,
            exclusive_start_key.as_ref(),
        );

        self.client
            .query(&request)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                base_labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Scan a single page of a table. Returns the items in the page along with the
    /// `last_evaluated_key`, if any, that should be passed back in as
    /// `exclusive_start_key` to fetch the next page. A `None` (or empty) returned key
    /// signals that the scan has reached the end of the table.
    #[tracing::instrument(name = "DynamoDbImpl::scan_page", skip_all)]
    pub async fn scan_page(
        &self,
        table_name: &Arc<str>,
        exclusive_start_key: Option<Item>,
        config: &ScanConfig,
    ) -> Result<ScanPage, DynamoError> {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("scan_page");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        let request = ScanRequest {
            table: table_name,
            limit: config.limit,
            segment: config.segment,
            total_segments: config.total_segments,
            exclusive_start_key: exclusive_start_key.as_ref(),
        };

        let output = self
            .client
            .scan(&request)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                base_labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)?;

        let last_evaluated_key = output.last_evaluated_key.filter(|key| !key.is_empty());
        Ok(ScanPage {
            items: output.items,
            last_evaluated_key,
        })
    }
}

/// One `BatchGetItem` request, for keys of one table.
#[lore_macro::test_pub]
trait BatchGetRequest: Clone + Send + Sync + 'static {
    /// Read `keys`, at most [`BATCH_GET_ITEM_MAX_COUNT`] of them, in one request.
    fn send(&self, keys: &[Item]) -> impl Future<Output = DynamoResult<BatchGetItemOutput>> + Send;
}

/// The requests of one [`DynamoDbImpl::batch_get_item`], each timed into the operation latency
/// histogram under the read's labels.
#[derive(Clone)]
struct ObservedBatchGet {
    client: DynamoClient,
    table: Arc<str>,
    consistent: bool,
    latency: Histogram<f64>,
    labels: LabelArray,
    slow_operation_duration: Duration,
}

impl BatchGetRequest for ObservedBatchGet {
    async fn send(&self, keys: &[Item]) -> DynamoResult<BatchGetItemOutput> {
        self.client
            .batch_get_item(&self.table, keys, self.consistent)
            .observe(
                self.latency.clone(),
                self.labels.clone(),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
    }
}

/// What a batch read, or one chunk of it, produced.
#[lore_macro::test_pub]
struct BatchRead {
    /// Every item that exists, or why the read failed.
    items: Result<Vec<Item>, DynamoError>,
    /// Requests sent again for keys left unprocessed.
    resends: u64,
}

/// Read `keys` through `request`, in chunks of at most [`BATCH_GET_ITEM_MAX_COUNT`] keys, each
/// chunk in a task of its own. Items come back in no particular order.
///
/// `DynamoDB` leaves keys unprocessed when the table sheds load or the response would pass sixteen
/// megabytes. A chunk resends them after the next wait of its own retry from `backoff`, so a shed
/// chunk backs off without holding up the others, and one ladder spans all of its rounds. A chunk
/// gives up once its retry has no wait left, and the read fails with [`AwsError::UnprocessedKeys`],
/// which the stores report as a back-off. The read never returns part of its answer, because a
/// caller takes a missing item to mean it does not exist.
#[lore_macro::test_pub]
async fn batch_get_chunks<R: BatchGetRequest>(
    request: R,
    mut keys: Vec<Item>,
    backoff: RetryPolicy,
) -> BatchRead {
    let mut tasks = JoinSet::new();

    while keys.len() > BATCH_GET_ITEM_MAX_COUNT {
        let chunk = keys.split_off(keys.len() - BATCH_GET_ITEM_MAX_COUNT);
        lore_base::lore_spawn!(
            tasks,
            drain_chunk(request.clone(), chunk, backoff.retry()).in_current_span()
        );
    }
    if !keys.is_empty() {
        lore_base::lore_spawn!(
            tasks,
            drain_chunk(request, keys, backoff.retry()).in_current_span()
        );
    }

    let mut items = Vec::new();
    let mut resends = 0;

    while let Some(joined) = tasks.join_next().await {
        let chunk = match joined {
            Ok(chunk) => chunk,
            Err(error) => {
                tracing::warn!(error = ?error, "Failed to join BatchGetItem task");
                return BatchRead {
                    items: Err(AwsError::JoinError),
                    resends,
                };
            }
        };

        resends += chunk.resends;
        match chunk.items {
            Ok(more) => append_items(&mut items, more),
            Err(error) => {
                return BatchRead {
                    items: Err(error),
                    resends,
                };
            }
        }
    }

    BatchRead {
        items: Ok(items),
        resends,
    }
}

/// Read one chunk through `request`, resending its unprocessed keys as [`batch_get_chunks`]
/// describes.
async fn drain_chunk<R: BatchGetRequest>(
    request: R,
    mut keys: Vec<Item>,
    mut backoff: Retry,
) -> BatchRead {
    let mut items = Vec::new();
    let mut resends = 0;

    loop {
        let output = match request.send(&keys).await {
            Ok(output) => output,
            Err(error) => {
                return BatchRead {
                    items: Err(AwsError::Aws(error)),
                    resends,
                };
            }
        };

        append_items(&mut items, output.items);

        let remaining = output.unprocessed_keys.len();
        if remaining == 0 {
            return BatchRead {
                items: Ok(items),
                resends,
            };
        }

        let Some(wait) = backoff.next_interval() else {
            tracing::warn!(
                remaining,
                resends,
                "DynamoDB kept leaving keys unprocessed; reporting the read as unanswered rather \
                 than as the part of it that arrived"
            );

            return BatchRead {
                items: Err(AwsError::UnprocessedKeys { remaining }),
                resends,
            };
        };

        tracing::trace!(remaining, "Resending unprocessed BatchGetItem keys");
        tokio::time::sleep(wait).await;
        resends += 1;
        keys = output.unprocessed_keys;
    }
}

/// Move `more` onto the end of `items`, taking `more` whole while `items` is still empty.
fn append_items(items: &mut Vec<Item>, mut more: Vec<Item>) {
    if items.is_empty() {
        *items = more;
    } else {
        items.append(&mut more);
    }
}

/// `items` in consecutive chunks of at most `size`, in order, each item moved once.
///
/// A list that fits one chunk is yielded as it is, without a copy, and an empty list yields
/// nothing.
fn into_chunks<T>(items: Vec<T>, size: usize) -> impl Iterator<Item = Vec<T>> {
    let (whole, rest) = if items.len() <= size {
        (Some(items).filter(|items| !items.is_empty()), Vec::new())
    } else {
        (None, items)
    };

    let mut rest = rest.into_iter();
    whole.into_iter().chain(std::iter::from_fn(move || {
        let chunk: Vec<T> = rest.by_ref().take(size).collect();
        (!chunk.is_empty()).then_some(chunk)
    }))
}

/// The request token of chunk `index` of a transaction sent under `token`.
///
/// Chunk 0 sends `token` itself, and each later chunk `token` with its index folded into the last
/// two bytes, so every chunk keeps one token across a caller's retries and no two of the first
/// 65,536 chunks share one.
#[lore_macro::test_pub]
fn chunk_token(token: Uuid, index: usize) -> Uuid {
    let mut bytes = token.into_bytes();
    bytes[14] ^= (index >> 8) as u8;
    bytes[15] ^= index as u8;
    Uuid::from_bytes(bytes)
}

/// The request for one page of `query`, resuming after `exclusive_start_key` when given.
fn query_request<'a, T: DynamoDbQuery>(
    table: &'a str,
    query: &'a T,
    names: &'a ExprNames,
    values: &'a ExprValues,
    exclusive_start_key: Option<&'a Item>,
) -> QueryRequest<'a> {
    QueryRequest {
        table,
        index: query.index_name(),
        key_condition: query.key_condition_expression(),
        names,
        values,
        filter: query.filter_expression(),
        limit: query.limit(),
        select: query.select(),
        consistent: query.consistent_read(),
        exclusive_start_key,
    }
}

/// Refuse a page whose cursor is the one its request resumed from.
///
/// `DynamoDB` advances the cursor whenever it reports one, so an unchanged cursor would have the
/// identical request sent again indefinitely. The failure is [`AwsError::StalledCursor`], which no
/// store reports as a back-off.
pub(crate) fn ensure_cursor_advanced(
    table: &str,
    pages: u64,
    resumed_from: Option<&Item>,
    next: &Item,
) -> Result<(), DynamoError> {
    if resumed_from != Some(next) {
        return Ok(());
    }

    tracing::warn!(
        table,
        pages,
        "DynamoDB reported another page without advancing its cursor; abandoning the read"
    );

    Err(AwsError::StalledCursor)
}

// We create an explicit mock, rather than relying on `automock` because we need to also mock a
// `Clone` impl. The downside of this is that we must explicitly replicate any signatures from the
// actual implementation into this block.
#[cfg(feature = "test-util")]
mock! {
    pub DynamoDb {
        pub fn new(
            client: DynamoClient,
            slow_operation_duration: Duration,
            metrics_attributes: Vec<KeyValue>,
            unprocessed_keys_retry: RetryPolicy,
        ) -> Self;

        pub async fn table_exists(
            &self,
            table_name: &Arc<str>,
        ) -> Result<bool, DynamoError>;

        #[tracing::instrument(name = "DynamoDbImpl::get_item", skip_all)]
        pub async fn get_item(
            &self,
            table_name: &Arc<str>,
            key: Item,
            consistent: bool,
        ) -> Result<GetItemOutput, DynamoError>;

        #[tracing::instrument(name = "DynamoDbImpl::item_exists", skip_all)]
        pub async fn item_exists(
            &self,
            table_name: &Arc<str>,
            key: Item,
            consistent: bool,
        ) -> Result<bool, DynamoError>;

        pub async fn batch_get_item(
            &self,
            table_name: &Arc<str>,
            items: Vec<Item>,
            consistent: bool,
        ) -> Result<Vec<Item>, DynamoError>;

        #[tracing::instrument(name = "DynamoDbImpl::put_item", skip_all)]
        pub async fn put_item(
            &self,
            table_name: &Arc<str>,
            item: Item,
        ) -> Result<WriteOutput, DynamoError>;

        #[tracing::instrument(name = "DynamoDbImpl::put_item_conditional", skip_all)]
        pub async fn put_item_conditional<T>(
            &self,
            table_name: &Arc<str>,
            item: Item,
            condition: T,
        ) -> Result<WriteOutput, DynamoError>
        where
            T: DynamoDbCondition + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::delete_item", skip_all)]
        pub async fn delete_item(
            &self,
            table_name: &Arc<str>,
            key: Item,
        ) -> Result<WriteOutput, DynamoError>;

        #[tracing::instrument(name = "DynamoDbImpl::delete_item_conditional", skip_all)]
        pub async fn delete_item_conditional<T>(
            &self,
            table_name: &Arc<str>,
            key: Item,
            condition: T,
        ) -> Result<WriteOutput, DynamoError>
        where
            T: DynamoDbCondition + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::transact_write_items", skip_all)]
        pub async fn transact_write_items(
            &self,
            table_name: &Arc<str>,
            items: Vec<TransactWriteItem>,
            token: Uuid,
        ) -> Result<Vec<WriteOutput>, DynamoError>;

        #[tracing::instrument(name = "DynamoDbImpl::query_single", skip_all)]
        pub async fn query_single<T>(
            &self,
            table_name: &Arc<str>,
            query: T,
        ) -> Result<QueryOutput, DynamoError>
        where
            T: DynamoDbQuery + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::query_paginated", skip_all)]
        pub async fn query_paginated<T>(
            &self,
            table_name: &Arc<str>,
            query: T,
        ) -> Result<QueryOutputAccumulation, DynamoError>
        where
            T: DynamoDbQuery + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::query_page", skip_all)]
        pub async fn query_page<T>(
            &self,
            table_name: &Arc<str>,
            query: T,
            exclusive_start_key: &Option<Item>,
        ) -> Result<QueryOutput, DynamoError>
        where
            T: DynamoDbQuery + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::scan_page", skip_all)]
        pub async fn scan_page(
            &self,
            table_name: &Arc<str>,
            exclusive_start_key: Option<Item>,
            config: &ScanConfig,
        ) -> Result<ScanPage, DynamoError>;
    }

    impl Clone for DynamoDb {
        fn clone(&self) -> Self;
    }
}

#[cfg(not(feature = "test-util"))]
pub use DynamoDbImpl as DynamoDb;
#[cfg(feature = "test-util")]
pub use MockDynamoDb as DynamoDb;
