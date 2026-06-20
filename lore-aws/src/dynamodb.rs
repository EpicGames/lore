// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::batch_get_item::BatchGetItemError;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemOutput;
use aws_sdk_dynamodb::operation::describe_table::DescribeTableError;
use aws_sdk_dynamodb::operation::get_item::GetItemError;
use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::put_item::PutItemOutput;
use aws_sdk_dynamodb::operation::query::QueryError;
use aws_sdk_dynamodb::operation::query::QueryOutput;
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
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::drop_record::DropRecord;
use lore_telemetry::drop_time::DropTimeMs;
use lore_telemetry::observe::Observe;
#[cfg(test)]
use mockall::mock;
use opentelemetry::KeyValue;
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
const METRICS_PUT_ITEM_CONDITIONAL_ATTRIBUTE: &str = "conditional";

const METRICS_BATCH_OPERATION_TOTAL_SIZE: &str = "batch_operation_size";
const SIZE_BOUNDARIES: [f64; 15] = [
    1., 5., 10., 50., 100., 200., 300., 500., 2_500., 5_000., 10_000., 20_000., 40_000., 60_000.,
    80_000.,
];

const METRICS_NESTED_API_CALLS: &str = "nested_api_calls.total";
const NESTED_CALL_VOLUMES: [f64; 11] = [1., 2., 3., 6., 10., 20., 40., 80., 100., 150., 200.];

const METRICS_UNPROCESSED_RETRIES: &str = "unprocessed_retries.total";
const UNPROCESSED_RETRIES_BOUNDARIES: [f64; 8] = [0., 1., 2., 4., 7., 12., 18., 25.];

const METRICS_QUERY_COUNT: &str = "query.accumulation.count";
const METRICS_QUERY_SCANNED_ROWS: &str = "query.accumulation.scanned_count";
const QUERY_ROWS: [f64; 10] = [
    10., 50., 100., 400., 1_200., 2_400., 7_200., 21_600., 64_800., 194_400.,
];

pub const METRICS_ACCUMULATION_OPERATION_LATENCY_METRIC_NAME: &str =
    "accumulation_operation_duration";
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

/// Encapsulate the components of a `DynamoDB` conditional put. See the `DynamoDB` docs for details.
/// <https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Expressions.ConditionExpressions.html>
pub trait DynamoDbPutCondition {
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
    instruments: DynamoDbInstruments,
}

impl DynamoDbImpl {
    pub fn new(
        client: aws_sdk_dynamodb::Client,
        slow_operation_duration: Duration,
        metrics_attributes: Vec<KeyValue>,
    ) -> Self {
        let instrument_provider = DynamoDbInstrumentProvider { metrics_attributes };

        Self {
            client,
            slow_operation_duration,

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
            Err(e) => Err(AwsError::AwsSdkError(e)),
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
            .map_err(AwsError::AwsSdkError)
    }

    pub async fn batch_get_item(
        &self,
        table_name: &Arc<str>,
        mut items: Vec<HashMap<String, AttributeValue>>,
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
        let mut unprocessed_retries = DropRecord::new(
            self.instruments.unprocessed_retries_histogram.clone(),
            &base_labels,
        );

        let mut join_set = JoinSet::new();
        let observe_fn = observe_aws_operation_callback(self.slow_operation_duration);
        let histogram = self.instruments.operation_latency_histogram.clone();

        // Dynamo has a cap on the number of items in a single batch, so we split the incoming items
        // into separate calls and spawn them all off in parallel into a join set.
        while !items.is_empty() {
            num_api_calls.add(1);

            let keys = items.split_off(items.len().saturating_sub(BATCH_GET_ITEM_MAX_COUNT));
            let keys_and_attributes = KeysAndAttributes::builder()
                .set_keys(Some(keys))
                .consistent_read(consistent)
                .build()
                .map_err(|e| {
                    tracing::warn!(
                        "Failed to build keys and attributes for Dynamo batch get item: {e:?}."
                    );
                    AwsError::BatchRequestError
                })?;
            let size = keys_and_attributes.keys.len();

            tracing::trace!("Sending batch of {} tasks to BatchGetItem", size);

            lore_base::lore_spawn!(
                join_set,
                self.client
                    .batch_get_item()
                    .request_items(&**table_name, keys_and_attributes)
                    .send()
                    .observe(histogram.clone(), base_labels.clone(), observe_fn)
                    .in_current_span()
            );
        }

        let mut output = Vec::new();

        // Consume the results as they come in. Note: we do not maintain the order of incoming
        // items in the returned output.
        while let Some(join_result) = join_set.join_next().await {
            let result = join_result.map_err(|e| {
                tracing::warn!("Failed to join BatchGetItem task: {e:?}");
                AwsError::JoinError
            })?;

            match result.output {
                Ok(o) => {
                    if let Some(mut items) = o
                        .responses
                        .and_then(|mut responses| responses.remove(&**table_name))
                    {
                        tracing::trace!("Got output from BatchGetItem: {items:?}");
                        output.append(&mut items);
                    }

                    // There are a variety of reasons that Dynamo might not process all keys in a
                    // single request (e.g. if the size of the returned items is too large, or if
                    // capacity is temporarily exceeded). If we got back any unprocessed keys create
                    // a new task and add it back into the join set.
                    if let Some(p) = o
                        .unprocessed_keys
                        .filter(|unprocessed| !unprocessed.is_empty())
                    {
                        unprocessed_retries.add(1);
                        tracing::trace!(
                            "Got unprocessed keys: {p:?}, re-submitting for next batch"
                        );

                        lore_base::lore_spawn!(
                            join_set,
                            self.client
                                .batch_get_item()
                                .set_request_items(Some(p))
                                .send()
                                .observe(histogram.clone(), base_labels.clone(), observe_fn)
                                .in_current_span()
                        );
                    }
                }
                Err(e) => return Err(AwsError::AwsSdkError(e)),
            }
        }

        Ok(output)
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
            {
                let mut labels = self
                    .instruments
                    .get_labels_for_operation_context(METRICS_PUT_ITEM_CONTEXT);
                labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
                labels.push(KeyValue::new(METRICS_PUT_ITEM_CONDITIONAL_ATTRIBUTE, false));
                labels
            },
            observe_aws_operation_callback(self.slow_operation_duration),
        )
        .await
        .output
        .map_err(AwsError::AwsSdkError)
    }

    #[tracing::instrument(name = "DynamoDbImpl::put_item_conditional", skip_all)]
    pub async fn put_item_conditional<T>(
        &self,
        table_name: &Arc<str>,
        item: HashMap<String, AttributeValue>,
        condition: T,
    ) -> Result<PutItemOutput, AwsError<SdkError<PutItemError>>>
    where
        T: DynamoDbPutCondition + 'static,
    {
        let ConditionParts {
            condition_expression,
            expression_names,
            expression_values,
        } = condition.into_parts();

        let mut put_item_request = self
            .client
            .put_item()
            .table_name(&**table_name)
            .set_item(Some(item))
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
            .condition_expression(condition_expression);

        for (k, v) in expression_names {
            put_item_request = put_item_request.expression_attribute_names(k, v);
        }

        for (k, v) in expression_values {
            put_item_request = put_item_request.expression_attribute_values(k, v);
        }

        put_item_request
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                {
                    let mut labels = self
                        .instruments
                        .get_labels_for_operation_context(METRICS_PUT_ITEM_CONTEXT);
                    labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
                    labels.push(KeyValue::new(METRICS_PUT_ITEM_CONDITIONAL_ATTRIBUTE, true));
                    labels
                },
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    #[tracing::instrument(name = "DynamoDbImpl::delete_item", skip_all)]
    pub async fn delete_item(
        &self,
        table_name: &Arc<str>,
        key: HashMap<String, AttributeValue>,
    ) -> Result<DeleteItemOutput, AwsError<SdkError<DeleteItemError>>> {
        let labels = {
            let mut labels = self
                .instruments
                .get_labels_for_operation_context("delete_item");
            labels.push(KeyValue::new(METRICS_TABLE_NAME_KEY, table_name.clone()));
            labels
        };

        self.client
            .delete_item()
            .table_name(&**table_name)
            .set_key(Some(key))
            .return_values(ReturnValue::None)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                labels,
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
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
        items: Vec<TransactWriteItem>,
    ) -> Result<Vec<TransactWriteItemsOutput>, AwsError<SdkError<TransactWriteItemsError>>> {
        let base_labels = self
            .instruments
            .get_labels_for_operation_context("transact_write_items");

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
                Err(e) => return Err(AwsError::AwsSdkError(e)),
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
        if matches!(query.select(), Some(Select::Count)) && query.limit().is_some() {
            panic!("Combining Select::Count and a limit is disallowed for paginated queries to avoid fetching count one row at a time.");
        }

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
                    if let Some(last_evaluated_key) = r.last_evaluated_key
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
                Err(e) => return Err(AwsError::AwsSdkError(e)),
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
            Err(e) => return Err(AwsError::AwsSdkError(e)),
        }
    }

    pub fn sdk_client(&self) -> &aws_sdk_dynamodb::Client {
        &self.client
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
            T: DynamoDbPutCondition + 'static;

        #[tracing::instrument(name = "DynamoDbImpl::delete_item", skip_all)]
        pub async fn delete_item(
            &self,
            table_name: &Arc<str>,
            key: HashMap<String, AttributeValue>,
        ) -> Result<DeleteItemOutput, AwsError<SdkError<DeleteItemError>>>;

        #[tracing::instrument(name = "DynamoDbImpl::transact_write_items", skip_all)]
        pub async fn transact_write_items(
            &self,
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
