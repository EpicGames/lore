// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::batch_get_item::BatchGetItemError;
use aws_sdk_dynamodb::operation::batch_get_item::BatchGetItemOutput;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemOutput;
use aws_sdk_dynamodb::operation::describe_table::DescribeTableError;
use aws_sdk_dynamodb::operation::get_item::GetItemError;
use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::put_item::PutItemOutput;
use aws_sdk_dynamodb::operation::query::QueryError;
use aws_sdk_dynamodb::operation::query::QueryOutput;
use aws_sdk_dynamodb::operation::scan::ScanError;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsOutput;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::KeysAndAttributes;
use aws_sdk_dynamodb::types::ReturnValue;
use aws_sdk_dynamodb::types::ReturnValuesOnConditionCheckFailure;
use aws_sdk_dynamodb::types::Select;
use aws_sdk_dynamodb::types::TransactWriteItem;
// Convenience exports
pub use aws_sdk_dynamodb::{Error as DynamoDbError, error, operation, primitives, types};
use lore_base::retry::Retry;
use lore_revision::util::time::RetryPolicy;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::drop_record::DropRecord;
use lore_telemetry::drop_time::DropTimeMs;
use lore_telemetry::observe::Observe;
#[cfg(test)]
use mockall::mock;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Histogram;
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::aws_error::AwsError;
use crate::dynamodb::query_output_accumulation::QueryOutputAccumulation;
use crate::observe_aws_operation_callback;

pub mod cancellation_reason;
pub mod query_output_accumulation;

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

pub trait DynamoDbQuery {
    fn index_name(&self) -> Option<String> {
        None
    }

    fn key_condition_expression(&self) -> &str;

    fn expression_attribute_names(&self) -> HashMap<String, String>;

    fn expression_attribute_values(&self) -> HashMap<String, AttributeValue>;

    fn filter_expression(&self) -> Option<String> {
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
    pub condition_expression: String,
    pub expression_names: HashMap<String, String>,
    pub expression_values: HashMap<String, AttributeValue>,
}

/// A condition as a request's setters take it: the expression, then the names and the values, an
/// empty map as none, since `DynamoDB` refuses a request whose names or values are empty.
type RequestCondition = (
    String,
    Option<HashMap<String, String>>,
    Option<HashMap<String, AttributeValue>>,
);

impl ConditionParts {
    fn into_request(self) -> RequestCondition {
        (
            self.condition_expression,
            (!self.expression_names.is_empty()).then_some(self.expression_names),
            (!self.expression_values.is_empty()).then_some(self.expression_values),
        )
    }
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
    pub items: Vec<HashMap<String, AttributeValue>>,
    pub last_evaluated_key: Option<HashMap<String, AttributeValue>>,
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

// This is the maximum number of items allowed in a single TransactWriteItems call, no need to make this
// configurable for now.
const TRANSACT_WRITE_ITEMS_CHUNKS: usize = 100;

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
    client: aws_sdk_dynamodb::Client,
    slow_operation_duration: Duration,
    /// The backoff for resending each `BatchGetItem` chunk's unprocessed keys.
    unprocessed_keys_retry: RetryPolicy,
    instruments: DynamoDbInstruments,
}

impl DynamoDbImpl {
    pub fn new(
        client: aws_sdk_dynamodb::Client,
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
    pub async fn table_exists(
        &self,
        table_name: &Arc<str>,
    ) -> Result<bool, AwsError<SdkError<DescribeTableError>>> {
        let labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("table_exists");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        match self
            .client
            .describe_table()
            .table_name(&**table_name)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
        {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(err)) if err.err().is_resource_not_found_exception() => {
                Ok(false)
            }
            Err(e) => Err(AwsError::sdk_error(e)),
        }
    }

    #[tracing::instrument(name = "DynamoDbImpl::get_item", skip_all)]
    pub async fn get_item(
        &self,
        table_name: &Arc<str>,
        key: HashMap<String, AttributeValue>,
        consistent: bool,
    ) -> Result<GetItemOutput, AwsError<SdkError<GetItemError>>> {
        let labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("get_item");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.client
            .get_item()
            .table_name(&**table_name)
            .set_key(Some(key))
            .consistent_read(consistent)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::sdk_error)
    }

    pub async fn batch_get_item(
        &self,
        table_name: &Arc<str>,
        items: Vec<HashMap<String, AttributeValue>>,
        consistent: bool,
    ) -> Result<Vec<HashMap<String, AttributeValue>>, AwsError<SdkError<BatchGetItemError>>> {
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
        num_api_calls.set(items.len().div_ceil(BATCH_GET_ITEM_MAX_COUNT) as u64);
        let mut telemetry = BatchGetTelemetry {
            latency: &self.instruments.operation_latency_histogram,
            labels: &base_labels,
            slow_operation_duration: self.slow_operation_duration,
            resends: DropRecord::new(
                self.instruments.unprocessed_retries_histogram.clone(),
                &base_labels,
            ),
        };
        let client = self.client.clone();
        let send = move |request| {
            client
                .batch_get_item()
                .set_request_items(Some(request))
                .send()
        };

        let result = batch_get_chunks(
            table_name,
            items,
            consistent,
            send,
            self.unprocessed_keys_retry,
            &mut telemetry,
        )
        .await;
        if matches!(result, Err(AwsError::UnprocessedKeys { .. })) {
            self.instruments
                .unprocessed_exhausted_counter
                .add(1, &base_labels);
        }
        result
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
        item: HashMap<String, AttributeValue>,
    ) -> Result<PutItemOutput, AwsError<SdkError<PutItemError>>> {
        let put = self
            .client
            .put_item()
            .table_name(&**table_name)
            .set_item(Some(item))
            .send();

        put.observe(
            self.instruments.operation_latency_histogram.clone(),
            self.write_labels(METRICS_PUT_ITEM_CONTEXT, table_name, false),
            observe_aws_operation_callback(self.slow_operation_duration),
        )
        .await
        .output
        .map_err(AwsError::sdk_error)
    }

    #[tracing::instrument(name = "DynamoDbImpl::put_item_conditional", skip_all)]
    pub async fn put_item_conditional<T>(
        &self,
        table_name: &Arc<str>,
        item: HashMap<String, AttributeValue>,
        condition: T,
    ) -> Result<PutItemOutput, AwsError<SdkError<PutItemError>>>
    where
        T: DynamoDbCondition + 'static,
    {
        let (expression, names, values) = condition.into_parts().into_request();
        self.client
            .put_item()
            .table_name(&**table_name)
            .set_item(Some(item))
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
            .condition_expression(expression)
            .set_expression_attribute_names(names)
            .set_expression_attribute_values(values)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_PUT_ITEM_CONTEXT, table_name, true),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::sdk_error)
    }

    #[tracing::instrument(name = "DynamoDbImpl::delete_item", skip_all)]
    pub async fn delete_item(
        &self,
        table_name: &Arc<str>,
        key: HashMap<String, AttributeValue>,
    ) -> Result<DeleteItemOutput, AwsError<SdkError<DeleteItemError>>> {
        self.client
            .delete_item()
            .table_name(&**table_name)
            .set_key(Some(key))
            .return_values(ReturnValue::None)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_DELETE_ITEM_CONTEXT, table_name, false),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::sdk_error)
    }

    /// Deletes the item at `key` when it meets `condition`. A refused condition fails with
    /// `ConditionalCheckFailedException` carrying the item as the condition found it, or no item
    /// when there was none.
    #[tracing::instrument(name = "DynamoDbImpl::delete_item_conditional", skip_all)]
    pub async fn delete_item_conditional<T>(
        &self,
        table_name: &Arc<str>,
        key: HashMap<String, AttributeValue>,
        condition: T,
    ) -> Result<DeleteItemOutput, AwsError<SdkError<DeleteItemError>>>
    where
        T: DynamoDbCondition + 'static,
    {
        let (expression, names, values) = condition.into_parts().into_request();
        self.client
            .delete_item()
            .table_name(&**table_name)
            .set_key(Some(key))
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
            .condition_expression(expression)
            .set_expression_attribute_names(names)
            .set_expression_attribute_values(values)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.write_labels(METRICS_DELETE_ITEM_CONTEXT, table_name, true),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::sdk_error)
    }

    #[allow(clippy::doc_markdown)]
    /// This method supports transactions that exceed DynamoDB's intrinsic restriction of 100 items
    /// per transaction. In scenarios where a transaction is supplied that contains more than this
    /// limit, the items will be chunked up into batches of 100 items, meaning that it's possible
    /// to have one batch succeed and the next batch fail. In cases where atomicity is needed across
    /// batches larger than 100 items, it is up to the caller to ensure this requirement.
    #[tracing::instrument(name = "DynamoDbImpl::transact_write_items", skip_all)]
    pub async fn transact_write_items(
        &self,
        table_name: &Arc<str>,
        items: Vec<TransactWriteItem>,
    ) -> Result<Vec<TransactWriteItemsOutput>, AwsError<SdkError<TransactWriteItemsError>>> {
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

        for chunk in items.chunks(TRANSACT_WRITE_ITEMS_CHUNKS) {
            num_api_calls.add(1);

            lore_base::lore_spawn!(
                join_set,
                self.client
                    .transact_write_items()
                    .set_transact_items(Some(chunk.to_vec()))
                    .send()
                    .observe(histogram.clone(), base_labels.clone(), observe_fn)
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
                Err(e) => return Err(AwsError::sdk_error(e)),
            }
        }

        Ok(output)
    }

    #[tracing::instrument(name = "DynamoDbImpl::query", skip_all)]
    pub async fn query_paginated<T>(
        &self,
        table_name: &Arc<str>,
        query: T,
    ) -> Result<QueryOutputAccumulation, AwsError<SdkError<QueryError>>>
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

        let mut join_set = JoinSet::new();
        let mut output = QueryOutputAccumulation::default();
        let histogram = self.instruments.operation_latency_histogram.clone();
        let observe_fn = observe_aws_operation_callback(self.slow_operation_duration);

        let mut make_query_task = |last_evaluated_key: Option<HashMap<String, AttributeValue>>| {
            num_api_calls.add(1);

            self.client
                .query()
                .table_name(&**table_name)
                .set_index_name(query.index_name())
                .set_limit(query.limit())
                .set_select(query.select())
                .set_filter_expression(query.filter_expression())
                .consistent_read(query.consistent_read())
                .key_condition_expression(query.key_condition_expression())
                .set_expression_attribute_names(Some(query.expression_attribute_names()))
                .set_expression_attribute_values(Some(query.expression_attribute_values()))
                .set_exclusive_start_key(last_evaluated_key)
                .send()
                .observe(histogram.clone(), base_labels.clone(), observe_fn)
        };

        lore_base::lore_spawn!(join_set, make_query_task(None).in_current_span());

        while let Some(join_result) = join_set.join_next().await {
            let result = join_result.map_err(|error| {
                tracing::warn!(error = ?error, "Failed to join query task");
                AwsError::JoinError
            })?;

            match result.output {
                Ok(r) => {
                    query_count.add(r.count() as u64);
                    query_scanned_count.add(r.scanned_count() as u64);

                    output.extend(r.items, r.count);

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
                    if *ENABLE_QUERY_PAGINATION
                        && let Some(last_evaluated_key) = r.last_evaluated_key
                        && !last_evaluated_key.is_empty()
                        && (query.limit().is_none()
                            || matches!(query.select(), Some(Select::Count))
                            || query.limit().is_some_and(|l| output.count < l))
                    {
                        lore_base::lore_spawn!(
                            join_set,
                            make_query_task(Some(last_evaluated_key)).in_current_span()
                        );
                    }
                }
                Err(e) => return Err(AwsError::sdk_error(e)),
            }
        }

        Ok(output)
    }

    #[tracing::instrument(name = "DynamoDbImpl::query_single", skip_all)]
    pub async fn query_single<T>(
        &self,
        table_name: &Arc<str>,
        query: T,
    ) -> Result<QueryOutput, AwsError<SdkError<QueryError>>>
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

        match self
            .client
            .query()
            .table_name(&**table_name)
            .set_index_name(query.index_name())
            .set_limit(query.limit())
            .set_select(query.select())
            .consistent_read(query.consistent_read())
            .key_condition_expression(query.key_condition_expression())
            .set_expression_attribute_names(Some(query.expression_attribute_names()))
            .set_expression_attribute_values(Some(query.expression_attribute_values()))
            .send()
            .observe(histogram.clone(), base_labels.clone(), observe_fn)
            .await
            .output
        {
            Ok(r) => {
                query_count.add(r.count() as u64);
                query_scanned_count.add(r.scanned_count() as u64);

                Ok(r)
            }
            Err(e) => return Err(AwsError::sdk_error(e)),
        }
    }

    /// Scan a single page of a table. Returns the items in the page along with the
    /// `last_evaluated_key`, if any, that should be passed back in as
    /// `exclusive_start_key` to fetch the next page. A `None` (or empty) returned key
    /// signals that the scan has reached the end of the table.
    #[tracing::instrument(name = "DynamoDbImpl::scan_page", skip_all)]
    pub async fn scan_page(
        &self,
        table_name: &Arc<str>,
        exclusive_start_key: Option<HashMap<String, AttributeValue>>,
        config: &ScanConfig,
    ) -> Result<ScanPage, AwsError<SdkError<ScanError>>> {
        let base_labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("scan_page");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        let output = self
            .client
            .scan()
            .table_name(&**table_name)
            .set_limit(config.limit)
            .set_segment(config.segment)
            .set_total_segments(config.total_segments)
            .set_exclusive_start_key(exclusive_start_key)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                base_labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::sdk_error)?;

        let items = output.items.unwrap_or_default();
        let last_evaluated_key = output.last_evaluated_key.filter(|key| !key.is_empty());
        Ok(ScanPage {
            items,
            last_evaluated_key,
        })
    }

    pub fn sdk_client(&self) -> &aws_sdk_dynamodb::Client {
        &self.client
    }
}

#[lore_macro::test_pub]
struct BatchGetTelemetry<'a> {
    /// Records the duration of each request, without the wait before a resend.
    latency: &'a Histogram<f64>,
    labels: &'a LabelArray,
    slow_operation_duration: Duration,
    /// Counts the resends across all chunks.
    resends: DropRecord<'a>,
}

/// Reads `keys` from `table_name` with `BatchGetItem` through `send`, in chunks of at most
/// [`BATCH_GET_ITEM_MAX_COUNT`] keys sent concurrently. Items come back in no particular order.
///
/// Each chunk uses its own `Retry` from `backoff` to resend unprocessed keys, so a throttled chunk
/// waits without delaying the others. The read either returns every item that exists or fails. It
/// never returns a partial result because a caller takes a missing item to mean the item does not
/// exist.
#[lore_macro::test_pub]
async fn batch_get_chunks<S, F>(
    table_name: &str,
    mut keys: Vec<HashMap<String, AttributeValue>>,
    consistent: bool,
    send: S,
    backoff: RetryPolicy,
    telemetry: &mut BatchGetTelemetry<'_>,
) -> Result<Vec<HashMap<String, AttributeValue>>, AwsError<SdkError<BatchGetItemError>>>
where
    S: Fn(HashMap<String, KeysAndAttributes>) -> F + Clone + Send + 'static,
    F: Future<Output = Result<BatchGetItemOutput, SdkError<BatchGetItemError>>> + Send + 'static,
{
    let mut tasks = JoinSet::new();

    while !keys.is_empty() {
        let chunk = keys.split_off(keys.len().saturating_sub(BATCH_GET_ITEM_MAX_COUNT));
        let request = KeysAndAttributes::builder()
            .set_keys(Some(chunk))
            .consistent_read(consistent)
            .build()
            .map_err(|e| {
                tracing::warn!(error = ?e, "Failed to build keys and attributes for Dynamo batch get item");
                AwsError::BatchRequestError
            })?;
        lore_base::lore_spawn!(
            tasks,
            send_chunk(
                None,
                send.clone(),
                HashMap::from([(table_name.to_owned(), request)]),
                backoff.retry(),
                telemetry,
            )
            .in_current_span()
        );
    }

    let mut output = Vec::new();

    loop {
        let Some(joined) = tasks.join_next().await else {
            break;
        };
        let (response, mut chunk_backoff) = joined.map_err(|e| {
            tracing::warn!(error = ?e, "Failed to join BatchGetItem task");
            AwsError::JoinError
        })?;
        let response = response.map_err(AwsError::sdk_error)?;

        if let Some(mut items) = response
            .responses
            .and_then(|mut responses| responses.remove(table_name))
        {
            output.append(&mut items);
        }

        // DynamoDB leaves keys unprocessed when the response would be too large or the table's
        // capacity is exceeded.
        let Some(unprocessed) = response
            .unprocessed_keys
            .filter(|unprocessed| !unprocessed.is_empty())
        else {
            continue;
        };

        let Some(wait) = chunk_backoff.next_interval() else {
            let remaining = unprocessed.values().map(|request| request.keys.len()).sum();
            return Err(AwsError::UnprocessedKeys { remaining });
        };

        telemetry.resends.add(1);
        tracing::trace!(keys = ?unprocessed, "Resending unprocessed BatchGetItem keys");

        lore_base::lore_spawn!(
            tasks,
            send_chunk(
                Some(wait),
                send.clone(),
                unprocessed,
                chunk_backoff,
                telemetry
            )
            .in_current_span()
        );
    }

    Ok(output)
}

/// Sends one `BatchGetItem` request through `send` once `wait` is over, and hands back `backoff`
/// for resending the request's unprocessed keys. The request's duration is recorded from the end
/// of the wait.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn send_chunk<S, F>(
    wait: Option<Duration>,
    send: S,
    request: HashMap<String, KeysAndAttributes>,
    backoff: Retry,
    telemetry: &BatchGetTelemetry<'_>,
) -> impl Future<
    Output = (
        Result<BatchGetItemOutput, SdkError<BatchGetItemError>>,
        Retry,
    ),
> + Send
+ 'static
where
    S: Fn(HashMap<String, KeysAndAttributes>) -> F + Send + 'static,
    F: Future<Output = Result<BatchGetItemOutput, SdkError<BatchGetItemError>>> + Send + 'static,
{
    let latency = telemetry.latency.clone();
    let labels = telemetry.labels.clone();
    let observe_fn = observe_aws_operation_callback(telemetry.slow_operation_duration);
    async move {
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
        let response = send(request).observe(latency, labels, observe_fn).await;
        (response.output, backoff)
    }
}

// We create an explicit mock, rather than relying on `automock` because we need to also mock a
// `Clone` impl. The downside of this is that we must explicitly replicate any signatures from the
// actual implementation into this block.
#[cfg(test)]
mock! {
    pub DynamoDb {
        pub fn new(
            client: aws_sdk_dynamodb::Client,
            slow_operation_duration: Duration,
            metrics_attributes: Vec<KeyValue>,
            unprocessed_keys_retry: RetryPolicy,
        ) -> Self;

        pub async fn table_exists(
            &self,
            table_name: &Arc<str>,
        ) -> Result<bool, AwsError<SdkError<DescribeTableError>>>;

        #[tracing::instrument(name = "DynamoDbImpl::get_item", skip_all)]
        pub async fn get_item(
            &self,
            table_name: &Arc<str>,
            key: HashMap<String, AttributeValue>,
            consistent: bool,
        ) -> Result<GetItemOutput, AwsError<SdkError<GetItemError>>>;

        pub async fn batch_get_item(
            &self,
            table_name: &Arc<str>,
            items: Vec<HashMap<String, AttributeValue>>,
            consistent: bool,
        ) -> Result<Vec<HashMap<String, AttributeValue>>, AwsError<SdkError<BatchGetItemError>>>;

        #[tracing::instrument(name = "DynamoDbImpl::put_item", skip_all)]
        pub async fn put_item(
            &self,
            table_name: &Arc<str>,
            item: HashMap<String, AttributeValue>,
        ) -> Result<PutItemOutput, AwsError<SdkError<PutItemError>>>;

        #[tracing::instrument(name = "DynamoDbImpl::put_item_conditional", skip_all)]
        pub async fn put_item_conditional<T>(
            &self,
            table_name: &Arc<str>,
            item: HashMap<String, AttributeValue>,
            condition: T,
        ) -> Result<PutItemOutput, AwsError<SdkError<PutItemError>>>
        where
            T: DynamoDbCondition + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::delete_item", skip_all)]
        pub async fn delete_item(
            &self,
            table_name: &Arc<str>,
            key: HashMap<String, AttributeValue>,
        ) -> Result<DeleteItemOutput, AwsError<SdkError<DeleteItemError>>>;

        #[tracing::instrument(name = "DynamoDbImpl::delete_item_conditional", skip_all)]
        pub async fn delete_item_conditional<T>(
            &self,
            table_name: &Arc<str>,
            key: HashMap<String, AttributeValue>,
            condition: T,
        ) -> Result<DeleteItemOutput, AwsError<SdkError<DeleteItemError>>>
        where
            T: DynamoDbCondition + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::transact_write_items", skip_all)]
        pub async fn transact_write_items(
            &self,
            table_name: &Arc<str>,
            items: Vec<TransactWriteItem>,
        ) -> Result<TransactWriteItemsOutput, AwsError<SdkError<TransactWriteItemsError>>>;

        #[tracing::instrument(name = "DynamoDbImpl::query_single", skip_all)]
        pub async fn query_single<T>(
            &self,
            table_name: &Arc<str>,
            query: T,
        ) -> Result<QueryOutput, AwsError<SdkError<QueryError>>>
        where
            T: DynamoDbQuery + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::query_paginated", skip_all)]
        pub async fn query_paginated<T>(
            &self,
            table_name: &Arc<str>,
            query: T,
        ) -> Result<QueryOutputAccumulation, AwsError<SdkError<QueryError>>>
        where
            T: DynamoDbQuery + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::scan_page", skip_all)]
        pub async fn scan_page(
            &self,
            table_name: &Arc<str>,
            exclusive_start_key: Option<HashMap<String, AttributeValue>>,
            config: &ScanConfig,
        ) -> Result<ScanPage, AwsError<SdkError<ScanError>>>;

        pub fn sdk_client(&self) -> &aws_sdk_dynamodb::Client;
    }

    impl Clone for DynamoDb {
        fn clone(&self) -> Self;
    }
}

#[cfg(not(test))]
pub use DynamoDbImpl as DynamoDb;
#[cfg(test)]
pub use MockDynamoDb as DynamoDb;
