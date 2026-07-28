// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::string::ToString;
use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::Select;
use aws_sdk_s3::operation::get_object::GetObjectError;
use bytes::Bytes;
use bytes::BytesMut;
use lore_base::error::AddressNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::TypedBytes;
use lore_revision::lore_warn;
use lore_revision::util::task_queue::METRICS_TASK_QUEUE_LABEL;
use lore_revision::util::task_queue::TaskQueue;
use lore_storage::ImmutableStore as ImmutableStoreTrait;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreObliterateStats;
use lore_storage::StoreQueryResult;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use serde::Deserialize;
use serde::Serialize;
use smallvec::SmallVec;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::aws_error::AwsError;
use crate::default_aws_timeout_millis;
use crate::dynamodb::ConditionParts;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbPutCondition;
use crate::dynamodb::DynamoDbQuery;
use crate::dynamodb::error::SdkError as DynamoDbSdkError;
use crate::s3::S3;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct FragmentsEntry {
    hash: Hash,
    #[serde(with = "serde_bytes")]
    repository_context: [u8; size_of::<Context>() * 2],
}

impl From<&FragmentsEntry> for Address {
    fn from(value: &FragmentsEntry) -> Self {
        Address {
            hash: value.hash,
            context: Context::from(&value.repository_context[size_of::<Context>()..]),
        }
    }
}

impl Debug for FragmentsEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FragmentsEntry")
            .field("hash", &self.hash)
            .field("repository_context", &hex::encode(self.repository_context))
            .finish()
    }
}

impl FragmentsEntry {
    fn new(repository: Context, address: Address) -> Self {
        let mut repository_context = [0u8; size_of::<Context>() * 2];
        repository_context[..size_of::<Context>()].copy_from_slice(repository.data());
        repository_context[size_of::<Context>()..].copy_from_slice(address.context.data());

        Self {
            hash: address.hash,
            repository_context,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct FragmentMetadataEntry {
    hash: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    fragment: Option<Fragment>,
}

impl FragmentMetadataEntry {
    fn new(hash: Hash) -> Self {
        Self {
            hash,
            fragment: None,
        }
    }

    fn with_fragment(mut self, fragment: Fragment) -> Self {
        self.fragment = Some(fragment);

        self
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct S3StoreSettings {
    pub bucket: String,
    pub endpoint_url: Option<String>,
    pub region: Option<String>,
    pub slow_operation_threshold_millis: u64,
    #[serde(default = "default_aws_timeout_millis")]
    pub timeout_millis: u64,
}

impl S3StoreSettings {
    pub fn new(bucket: String) -> Self {
        Self {
            bucket,
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

    pub fn with_region(mut self, region: String) -> Self {
        self.region = Some(region);
        self
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DynamoDbImmutableStoreSettings {
    pub fragments_table_name: String,
    pub metadata_table_name: String,
    pub endpoint_url: Option<String>,
    pub region: Option<String>,
    pub slow_operation_threshold_millis: u64,
    #[serde(default = "default_aws_timeout_millis")]
    pub timeout_millis: u64,
}

impl DynamoDbImmutableStoreSettings {
    pub fn new(fragments_table_name: String, metadata_table_name: String) -> Self {
        Self {
            fragments_table_name,
            metadata_table_name,
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

/// The maximum number of individual exists tasks we'll allow to be submitted across all concurrent
/// requests.
fn default_submission_limit() -> usize {
    150_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct AwsImmutableStoreSettings {
    pub s3: S3StoreSettings,
    pub dynamodb: DynamoDbImmutableStoreSettings,
    #[serde(default)]
    pub force_write: bool,
    #[serde(default = "default_submission_limit")]
    pub batch_exist_submission_limit: usize,
}

impl AwsImmutableStoreSettings {
    pub fn new(
        s3: S3StoreSettings,
        dynamodb: DynamoDbImmutableStoreSettings,
        force_write: bool,
    ) -> Self {
        Self {
            s3,
            dynamodb,
            force_write,
            batch_exist_submission_limit: default_submission_limit(),
        }
    }
}

pub const FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE: &str = "hash";
pub const FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE: &str = "repository_context";

#[derive(Debug, Clone, PartialEq)]
enum FragmentsQuery {
    Repository(Hash, Context),
    Hash(Hash),
    HashCount(Hash),
}

impl DynamoDbQuery for FragmentsQuery {
    fn key_condition_expression(&self) -> &str {
        match self {
            FragmentsQuery::Repository(_, _) => "#pk = :hash and begins_with(#sk, :repository)",
            FragmentsQuery::Hash(_) | FragmentsQuery::HashCount(_) => "#pk = :hash",
        }
    }

    fn expression_attribute_names(&self) -> HashMap<String, String> {
        match self {
            FragmentsQuery::Repository(_, _) => HashMap::from([
                (
                    "#pk".to_string(),
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                ),
                (
                    "#sk".to_string(),
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                ),
            ]),
            FragmentsQuery::Hash(_) | FragmentsQuery::HashCount(_) => HashMap::from([(
                "#pk".to_string(),
                FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
            )]),
        }
    }

    fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
        match self {
            FragmentsQuery::Repository(hash, repository) => HashMap::from([
                (
                    ":hash".to_string(),
                    AttributeValue::B(Blob::new(hash.data())),
                ),
                (
                    ":repository".to_string(),
                    AttributeValue::B(Blob::new(repository.data())),
                ),
            ]),
            FragmentsQuery::Hash(hash) | FragmentsQuery::HashCount(hash) => HashMap::from([(
                ":hash".to_string(),
                AttributeValue::B(Blob::new(hash.data())),
            )]),
        }
    }

    fn limit(&self) -> Option<i32> {
        match self {
            FragmentsQuery::Repository(_, _) | FragmentsQuery::Hash(_) => Some(1),
            FragmentsQuery::HashCount(_) => None,
        }
    }

    fn select(&self) -> Option<Select> {
        match self {
            FragmentsQuery::Repository(_, _) | FragmentsQuery::Hash(_) => None,
            FragmentsQuery::HashCount(_) => Some(Select::Count),
        }
    }

    fn consistent_read(&self) -> bool {
        matches!(self, FragmentsQuery::HashCount(_))
    }
}

#[derive(Debug, PartialEq)]
struct UpdateMetadataCondition(Fragment);

impl DynamoDbPutCondition for UpdateMetadataCondition {
    fn into_parts(self) -> ConditionParts {
        ConditionParts {
            condition_expression: "#flags = :flags AND #size_payload = :size_payload AND #size_content = :size_content".to_string(),
            expression_names: HashMap::from([
                ("#flags".to_string(), "flags".to_string()),
                ("#size_payload".to_string(), "size_payload".to_string()),
                ("#size_content".to_string(), "size_content".to_string()),
            ]),
            expression_values: HashMap::from([
                (
                    ":flags".to_string(),
                    AttributeValue::N(self.0.flags.to_string()),
                ),
                (
                    ":size_payload".to_string(),
                    AttributeValue::N(self.0.size_payload.to_string()),
                ),
                (
                    ":size_content".to_string(),
                    AttributeValue::N(self.0.size_content.to_string()),
                ),
            ]),
        }
    }
}

static STORE_ATTRIBUTES: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("store", "aws")]);

type BatchTaskResult = Result<(usize, StoreMatch), (usize, StoreError)>;

struct GetS3objectContentsOutput {
    read: usize,
    bytes: BytesMut,
}

pub struct AwsImmutableStore {
    s3: S3,
    dynamodb: DynamoDb,
    task_queue: TaskQueue<BatchTaskResult>,
    bucket: String,
    fragments_table_name: Arc<str>,
    metadata_table_name: Arc<str>,
    force_write: bool,
    latency_histogram: Histogram<f64>,
    labels_get: LabelArray,
    labels_put: LabelArray,
    labels_exist: LabelArray,
    labels_exist_batch: LabelArray,
    labels_obliterate: LabelArray,
    labels_query: LabelArray,
    labels_copy: LabelArray,
}

impl AwsImmutableStore {
    pub fn new(s3: S3, dynamodb: DynamoDb, settings: &AwsImmutableStoreSettings) -> Self {
        let provider = AwsImmutableStoreInstrumentProvider;

        let latency_histogram =
            provider.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME);
        let labels_exist = provider.get_labels_for_operation_context("exist");
        let labels_get = provider.get_labels_for_operation_context("get");
        let labels_put = provider.get_labels_for_operation_context("put");
        let labels_exist_batch = provider.get_labels_for_operation_context("exist_batch");
        let labels_obliterate = provider.get_labels_for_operation_context("obliterate");
        let labels_query = provider.get_labels_for_operation_context("query");
        let labels_copy = provider.get_labels_for_operation_context("copy");
        Self {
            s3,
            dynamodb,
            task_queue: TaskQueue::new(
                u32::MAX,
                Semaphore::MAX_PERMITS,
                settings.batch_exist_submission_limit,
                vec![KeyValue::new(
                    METRICS_TASK_QUEUE_LABEL,
                    "store.immutable.aws",
                )],
            ),
            bucket: settings.s3.bucket.clone(),
            fragments_table_name: Arc::from(settings.dynamodb.fragments_table_name.clone()),
            metadata_table_name: Arc::from(settings.dynamodb.metadata_table_name.clone()),
            force_write: settings.force_write,
            latency_histogram,
            labels_get,
            labels_put,
            labels_exist,
            labels_exist_batch,
            labels_obliterate,
            labels_query,
            labels_copy,
        }
    }

    async fn exists_exact(&self, entry: &FragmentsEntry) -> Result<bool, StoreError> {
        let item = serde_dynamo::to_item(entry).map_err(|e| {
            warn!(
                "Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e:?}",
            );
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment entry for DynamoDB lookup",
            )
        })?;

        let output = self
            .dynamodb
            .get_item(
                &self.fragments_table_name,
                item,
                true, /* consistent read */
            )
            .await
            .map_err(|e| {
                warn!("DynamoDb lookup for fragment entry failed for {entry:?}: {e:?}");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment lookup failed")
                }
            })?;

        Ok(output.item.is_some())
    }

    async fn exists_repository(&self, entry: &FragmentsEntry) -> Result<bool, StoreError> {
        let repo = Context::from(&entry.repository_context[..size_of::<Context>()]);

        self.dynamodb
            .query_single(
                &self.fragments_table_name,
                FragmentsQuery::Repository(entry.hash, repo),
            )
            .await
            .map(|output| output.count > 0)
            .map_err(|e| {
                warn!(
                    "DynamoDb query for fragment entry by hash and repo failed for {entry:?}: {e:?}"
                );
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(
                        e,
                        "DynamoDB fragment query by repository failed",
                    )
                }
            })
    }

    async fn exists_hash(&self, entry: &FragmentsEntry) -> Result<bool, StoreError> {
        self.dynamodb
            .query_single(&self.fragments_table_name, FragmentsQuery::Hash(entry.hash))
            .await
            .map(|output| output.count > 0)
            .map_err(|e| {
                warn!("DynamoDb query for fragment entry by hash failed for {entry:?}: {e:?}");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment query by hash failed")
                }
            })
    }

    async fn ensure_exists(
        &self,
        repository: Context,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(), StoreError> {
        if !self.exists(repository, address, match_required).await? {
            return Err(StoreError::from(AddressNotFound::from(address)));
        }

        Ok(())
    }

    async fn exists(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<bool, StoreError> {
        if match_requested == StoreMatch::MatchNone {
            return Ok(false);
        }

        let key = FragmentsEntry::new(repository, address);

        match match_requested {
            StoreMatch::MatchFull => self.exists_exact(&key).await,
            StoreMatch::MatchPartition => self.exists_repository(&key).await,
            StoreMatch::MatchHash => self.exists_hash(&key).await,
            StoreMatch::MatchNone => Ok(false),
        }.inspect(|matched| {
            if !matched {
                debug!("Fragment does not exist for repository: {repository} and address: {address} with match required: {match_requested:?}.");
            }
        })
    }

    // Performs an existence check for a batch of addresses at the `MatchFull` level. This means we
    // can use `BatchGetItem` to reduce the number of Dynamo calls we need to have in flight at
    // once.
    async fn exist_batch_exact(
        &self,
        repository: Context,
        addresses: &[Address],
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let mut items = Vec::with_capacity(addresses.len());

        let mut address_index_map = HashMap::new();

        for (pos, address) in addresses.iter().enumerate() {
            let address = *address;

            address_index_map.insert(address, pos);

            let entry = FragmentsEntry::new(repository, address);
            items.push(serde_dynamo::to_item(&entry).map_err(|e| {
                warn!(
                    "Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e:?}",
                );
                StoreError::internal_with_context(e, "Failed to serialize fragment entry for DynamoDB batch lookup")
            })?);
        }

        let output = self
            .dynamodb
            .batch_get_item(
                &self.fragments_table_name,
                items,
                true, /* consistent read */
            )
            .await
            .map_err(|err| {
                warn!("DynamoDb batch exists failed: {err:?}");
                if matches!(&err, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    warn!("DynamoDb batch exists failed addresses: {addresses:?}");
                    StoreError::internal_with_context(err, "DynamoDB batch get items failed")
                }
            })?;

        let mut result: Vec<StoreMatch> = addresses.iter().map(|_| StoreMatch::MatchNone).collect();

        for item in output {
            match serde_dynamo::from_item::<HashMap<String, AttributeValue>, FragmentsEntry>(item) {
                Ok(entry) => match address_index_map.get(&((&entry).into())) {
                    Some(pos) => result[*pos] = StoreMatch::MatchFull,
                    None => {
                        warn!(
                            "Found entry in batch get item result that didn't exist in the input addresses? {entry:?}"
                        );
                    }
                },
                Err(e) => {
                    warn!("Failed to convert dynamo item to fragments entry: {e:?}");
                }
            }
        }

        Ok(result)
    }

    // Performs an existence check for a batch of addresses at either the `MatchHash` or
    // `MatchPartition` level. Any other value for `match_requested` will result in an error. This
    // method will perform individual DynamoDb queries for each provided address, limiting the
    // number of submitted tasks via a `TaskQueue` with a submission limit in place in order to
    // enforce an upper bound on memory usage when checking the existence of a large number of
    // fragments concurrently.
    async fn exist_batch_inexact(
        &self,
        repository: Context,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        if matches!(
            match_requested,
            StoreMatch::MatchNone | StoreMatch::MatchFull
        ) {
            warn!("Invalid match requested for exist_batch_internal: {match_requested:?}");
            return Err(StoreError::internal(
                "Invalid match type for batch inexact exist (must be Hash or Repository)",
            ));
        }

        let mut join_set = JoinSet::new();

        let dynamodb = self.dynamodb.clone();
        for (pos, address) in addresses.iter().enumerate() {
            let dynamodb = dynamodb.clone();
            let address = *address;

            let table_name = self.fragments_table_name.clone();
            let task = async move {
                match match_requested {
                    StoreMatch::MatchPartition => dynamodb.query_single(
                        &table_name,
                        FragmentsQuery::Repository(address.hash, repository),
                    ),
                    StoreMatch::MatchHash => dynamodb.query_single(
                        &table_name,
                        FragmentsQuery::Hash(address.hash),
                    ),
                    _ => {
                        // We've already checked for the other match types above, so we should never
                        // reach this
                        error!("Invalid match requested: {match_requested:?}");
                        unreachable!();
                    }
                }.await
                    .map(|output| (pos, if output.count > 0 { match_requested } else { StoreMatch::MatchNone }))
                    .map_err(|e| {
                        warn!(
                            "DynamoDb query for fragment entry by hash and repo failed for repository: {repository} and address: {address}: {e:?}"
                        );
                        if matches!(&e, AwsError::AwsSdkError(_)) {
                            (pos, StoreError::from(SlowDown))
                        } else {
                            (pos, StoreError::internal_with_context(e, "DynamoDB query for batch inexact exist failed"))
                        }
                    })
            }.in_current_span();

            lore_base::lore_spawn!(
                join_set,
                self.task_queue
                    .submit(Box::pin(task))
                    .await
                    .map_err(|err| {
                        lore_warn!("Task queue error: {err}");
                        StoreError::internal_with_context(
                            err,
                            "Failed to submit batch inexact exist task",
                        )
                    })?
                    .in_current_span()
            );
        }

        let mut output: Vec<StoreMatch> = addresses.iter().map(|_| StoreMatch::MatchNone).collect();

        while let Some(join_result) = join_set.join_next().await {
            if let Err(e) = join_result {
                warn!("Failed to join exist batch task, falling back to no match {e:?}");
                continue;
            }

            let result = join_result.unwrap().map_err(|e| {
                // If the task queue itself failed, something has gone terribly wrong.
                error!("TaskQueue failure: {e:?}");
                StoreError::internal_with_context(
                    e,
                    "Failed to process batch inexact exist results",
                )
            })?;

            match result {
                Ok((pos, m)) => output[pos] = m,
                Err((pos, e)) => {
                    // If an individual check failed, log the error and continue on, using the
                    // default `MatchNone` that was prepopulated for the index.
                    warn!(
                        "Failed to check existence for address {} in repository {repository}: {e:?}",
                        addresses[pos]
                    );
                }
            }
        }

        Ok(output)
    }

    async fn lookup(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        let mut match_requested = match_requested;
        let mut exists = self.exists(repository, address, match_requested).await?;

        // If a full match was requested but not found, short circuit. Since we do not currently
        // support partial uploads there's no benefit to checking to see if a match exists at any
        // other granularity.
        // TODO(jcohen): If we decide to re-add support for partial uploads, this will need to be
        //  removed.
        if !exists && match_requested == StoreMatch::MatchFull {
            return Ok(StoreMatch::MatchNone);
        }

        while !exists && match_requested.prev().is_some() {
            match_requested = match_requested.prev().unwrap();
            exists = self.exists(repository, address, match_requested).await?;
        }

        Ok(if exists {
            match_requested
        } else {
            StoreMatch::MatchNone
        })
    }

    async fn do_query(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
        hide_obliterates: bool,
    ) -> Result<StoreQueryResult, StoreError> {
        let match_made = self.lookup(repository, address, match_requested).await?;

        if match_made == StoreMatch::MatchNone {
            return Ok(StoreQueryResult {
                fragment: Fragment::default(),
                match_made,
            });
        }

        let fragment = self.load_metadata(address.hash).await.map_err(|e| {
            warn!(
                "Load metadata failed for address: {address:?} in repository: {repository:?}: {e:?}"
            );
            StoreError::internal_with_context(e, "Failed to load metadata after fragment lookup")
        })?;

        if (fragment.flags & FragmentFlags::PayloadObliteration) != 0 && hide_obliterates {
            debug!("Query found obliterated fragment at address {address}");
            Ok(StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone,
            })
        } else {
            Ok(StoreQueryResult {
                fragment,
                match_made,
            })
        }
    }

    async fn write_metadata(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
    ) -> Result<(), StoreError> {
        let metadata = FragmentMetadataEntry::new(address.hash).with_fragment(fragment);
        let item = serde_dynamo::to_item(&metadata).map_err(|e| {
            warn!("Failed to serialize metadata entry for repository: {repository:?} and address: {address:?} to dynamo av map: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize metadata for DynamoDB write")
        })?;

        self.dynamodb.put_item(&self.metadata_table_name, item).await.map_err(|e| {
            warn!("Failed to save metadata entry for repository: {repository:?} and address: {address:?}: {e:?}");
            if matches!(&e, AwsError::AwsSdkError(_)) {
                StoreError::from(SlowDown)
            } else {
                StoreError::internal_with_context(e, "DynamoDB metadata write failed")
            }
        })?;

        Ok(())
    }

    async fn update_metadata(
        &self,
        address: Address,
        updated: Fragment,
        expected: Fragment,
    ) -> Result<(), StoreError> {
        let metadata = FragmentMetadataEntry::new(address.hash).with_fragment(updated);
        let item = serde_dynamo::to_item(&metadata).map_err(|e| {
            warn!("Failed to serialize metadata entry for fragment with address: {address}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize metadata for DynamoDB update")
        })?;

        let result = self
            .dynamodb
            .put_item_conditional(
                &self.metadata_table_name,
                item,
                UpdateMetadataCondition(expected),
            )
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(AwsError::AwsSdkError(DynamoDbSdkError::ServiceError(err)))
                if err.err().is_conditional_check_failed_exception() =>
            {
                if let PutItemError::ConditionalCheckFailedException(e) = err.err() {
                    match e.item() {
                        Some(item) => {
                            let entry: Option<FragmentMetadataEntry> =
                                serde_dynamo::from_item(item.to_owned())
                                    .inspect_err(|e| {
                                        warn!("Failed to parse fragment from item: {item:?}: {e}");
                                    })
                                    .ok();

                            warn!(
                                "Failed to update metadata, expected metadata: {expected:?} did not match actual: {:?}",
                                entry
                            );
                        }
                        None => {
                            warn!(
                                "Failed to update metadata, no existing metadata found for {address}"
                            );
                        }
                    }
                    Err(StoreError::internal(
                        "Failed to update metadata due to conflict",
                    ))
                } else {
                    unreachable!()
                }
            }
            Err(e) => {
                warn!(
                    "DynamoDB conditional put failed while updating metadata for {address}: {e:?}"
                );
                Err(StoreError::internal_with_context(
                    e,
                    "DynamoDB conditional metadata update failed",
                ))
            }
        }
    }

    async fn associate_fragment(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError> {
        let entry = FragmentsEntry::new(repository, address);

        let item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment association for DynamoDB",
            )
        })?;

        self.dynamodb.put_item(&self.fragments_table_name, item).await
            .map_err(|e| {
                warn!({REPOSITORY_ID} = %repository, {ADDRESS} = %address, error = ?e, "Failed to put item while storing fragment association");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment association write failed")
                }
            })?;

        Ok(())
    }

    async fn has_associations(&self, hash: Hash) -> Result<bool, StoreError> {
        self.dynamodb
            .query_single(&self.fragments_table_name, FragmentsQuery::HashCount(hash))
            .await
            .map(|output| output.count > 0)
            .map_err(|e| {
                warn!(
                    "DynamoDb query for fragment association count failed for hash {hash}: {e:?}"
                );
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(
                        e,
                        "DynamoDB fragment association count query failed",
                    )
                }
            })
    }

    async fn delete_association(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError> {
        let entry = FragmentsEntry::new(repository, address);

        let item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment association for DynamoDB delete",
            )
        })?;

        self.dynamodb
            .delete_item(&self.fragments_table_name, item)
            .await
            .map_err(|e| {
                warn!("Failed to delete fragment association for repository: {repository} and address: {address}: {e:?}");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment association delete failed")
                }
            })?;

        Ok(())
    }

    async fn write_payload(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
        payload: Bytes,
    ) -> Result<(), StoreError> {
        if payload.len() != fragment.size_payload as usize {
            warn!(
                "Failed to write fragment to immutable store for address: {address}, payload size invalid (expected {} bytes, but got {})",
                fragment.size_payload,
                payload.len()
            );
            return Err(StoreError::internal(format!(
                "Failed to store in immutable store for put {}",
                address.hash
            )));
        }

        let mut dst = [0u8; 64];
        let hash = lore_revision::util::to_hex_str(address.hash.data(), &mut dst);

        self.s3
            .put_object(self.bucket.as_str(), hash, payload.to_vec())
            .await
            .map(|_| ())
            .map_err(|e| {
                warn!("Failed to write payload for hash: {}: {e:?}", address.hash);
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "S3 put object failed")
                }
            })?;

        // Writing metadata is not tied to writing the payload to S3, which means that over time
        // we'll likely wind up in a scenario where some fragments exist in S3, but their associated
        // metadata does not exist in Dynamo. In this scenario, a later query and/or read for the
        // fragment would treat it as not found, prompting clients to resend the fragment. This
        // means that whenever we land in this scenario, we should be self-healing.
        self.write_metadata(repository, address, fragment).await?;

        self.associate_fragment(repository, address).await?;

        Ok(())
    }

    /// Permanently delete a payload from S3 by removing *ALL* versions from the bucket.
    async fn delete_payload(&self, hash: Hash) -> Result<(), StoreError> {
        let mut dst = [0u8; 64];
        let hash = lore_revision::util::to_hex_str(hash.data(), &mut dst);

        let versions: Option<Vec<Option<String>>> = self
            .s3
            .list_versions(self.bucket.as_str(), hash)
            .await
            .map(|output| {
                output
                    .versions
                    .map(|v| v.iter().map(|v| v.version_id.clone()).collect())
            })
            .map_err(|e| {
                warn!("Failed to list versions for hash: {hash}: {e:?}");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "S3 list object versions failed")
                }
            })?;

        if let Some(versions) = versions {
            for version in versions {
                self.s3
                    .delete_object(self.bucket.as_str(), hash, version)
                    .await
                    .map_err(|e| {
                        warn!("Failed to delete payload for hash: {hash}: {e:?}");
                        if matches!(&e, AwsError::AwsSdkError(_)) {
                            StoreError::from(SlowDown)
                        } else {
                            StoreError::internal_with_context(e, "S3 delete object version failed")
                        }
                    })?;
            }
        } else {
            self.s3
                .delete_object(self.bucket.as_str(), hash, None)
                .await
                .map_err(|e| {
                    warn!("Failed to delete payload for hash: {hash}: {e:?}");
                    if matches!(&e, AwsError::AwsSdkError(_)) {
                        StoreError::from(SlowDown)
                    } else {
                        StoreError::internal_with_context(e, "S3 delete object failed")
                    }
                })?;
        }

        Ok(())
    }

    /// Loads fragment metadata, with just size validation
    async fn metadata_with_size_validation(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let metadata = self.load_metadata(hash).await?;
        // Reject upfront before issuing the S3 GET: a corrupt metadata entry
        // could declare a payload larger than the protocol threshold, which
        // would then be happily extended into the in-memory buffer below.
        lore_storage::validate_fragment_size(&metadata)?;
        Ok(metadata)
    }

    /// Loads fragment metadata, applying all validation
    /// to ensure it is a valid fragment to load
    async fn metadata_with_load_validation(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let metadata = self.metadata_with_size_validation(hash).await?;

        if (metadata.flags & FragmentFlags::PayloadObliteration) != 0 {
            return Err(StoreError::from(AddressNotFound::from(
                Address::zero_context_hash(hash),
            )));
        };

        Ok(metadata)
    }

    async fn load_metadata(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let item = serde_dynamo::to_item(FragmentMetadataEntry::new(hash)).map_err(|e| {
            warn!("Failed to serialize fragment metadata entry for {hash}: {e:?}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment entry for DynamoDB metadata load",
            )
        })?;

        let metadata: FragmentMetadataEntry = if let Some(av_map) = self
            .dynamodb
            .get_item(
                &self.metadata_table_name,
                item,
                true, /* consistent read */
            )
            .await
            .map_err(|e| {
                warn!(%hash, ?e, "Failed to get fragment metadata for hash");
                if let AwsError::AwsSdkError(sdk_error) = e
                    && let SdkError::TimeoutError(_) = sdk_error
                {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
                }
            })?
            .item
        {
            serde_dynamo::from_item(av_map).map_err(|e| {
                warn!("Failed to deserialize fragment metadata: {e:?}");
                StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
            })
        } else {
            warn!("Failed to get metadata for fragment, no item found");
            Err(StoreError::from(AddressNotFound::from(
                Address::zero_context_hash(hash),
            )))
        }?;

        metadata.fragment.ok_or_else(|| {
            warn!("No fragment found on metadata from store: {metadata:?}");
            StoreError::internal("Fragment metadata entry missing fragment field")
        })
    }

    async fn get_s3_object_contents(
        &self,
        hash: Hash,
    ) -> Result<GetS3objectContentsOutput, StoreError> {
        let mut dst = [0u8; 64];
        let mut output = self
            .s3
            .get_object(
                self.bucket.as_str(),
                lore_revision::util::to_hex_str(hash.data(), &mut dst),
                None,
            )
            .await
            .map_err(|e| {
                if let AwsError::AwsSdkError(sdk_error) = e {
                    debug!(hash = %hash, error = ?sdk_error, "get_s3_payload SDK error getting object");
                    match sdk_error.into_service_error() {
                        GetObjectError::NoSuchKey(_) => StoreError::from(AddressNotFound::from(
                            Address::zero_context_hash(hash),
                        )),
                        _ => StoreError::from(SlowDown),
                    }
                } else {
                    debug!(hash = %hash, error = ?e, "get_s3_payload failed to get object");
                    StoreError::internal_with_context(e, "S3 get object failed")
                }
            })?;

        let mut buffer = BytesMut::with_capacity(FRAGMENT_SIZE_THRESHOLD);
        let mut read = 0_usize;
        while let Some(bytes) = output.body.next().await {
            let bytes = bytes.map_err(|e| {
                warn!("Failed to read bytes from S3 response for key: {hash}: {e:?}");
                StoreError::internal_with_context(e, "Failed to read bytes from S3 response stream")
            })?;
            read += bytes.len();
            trace!("Read {read} bytes from S3 stream");

            buffer.extend_from_slice(bytes.as_ref());
        }
        trace!("Total read {read} bytes from S3 stream");

        Ok(GetS3objectContentsOutput {
            bytes: buffer,
            read,
        })
    }

    fn read_payload(
        &self,
        mut s3_contents: GetS3objectContentsOutput,
        hash: Hash,
        fragment: Fragment,
    ) -> Result<Bytes, StoreError> {
        let payload_size = fragment.size_payload as usize;
        let buffer_size = s3_contents.bytes.len();

        // This exists to work around an inconsistency that can occur as we switch from storing
        // metadata prefixed to objects in S3 to storing metadata separately in Dynamo. If the
        // amount of data we read does not match the expected size, we should fail the request.
        // However, if it's off by exactly the size of fragment metadata, and we're in force-write
        // mode, assume it's ok.
        let buffer = if buffer_size > payload_size
            && (buffer_size - payload_size) == size_of::<Fragment>()
            && self.force_write
        {
            s3_contents.bytes.split_off(size_of::<Fragment>()).freeze()
        } else {
            s3_contents.bytes.freeze()
        };

        if buffer_size == payload_size {
            Ok(buffer)
        } else {
            warn!(
                "Wrong number of bytes read from payload, expected {payload_size} but got {buffer_size}, from a total of {} bytes read",
                s3_contents.read
            );
            Err(StoreError::internal(format!(
                "Failed to load from immutable store, size mismatch (load {buffer_size}, expected {payload_size}) for get {hash}"
            )))
        }
    }

    async fn load(&self, hash: Hash) -> Result<(Fragment, Bytes), StoreError> {
        // Run both futures concurrently. The select! loop breaks as soon as metadata resolves.
        // If S3 finishes first its result is stashed, and we keep waiting for metadata.
        let metadata_fut = self.metadata_with_load_validation(hash);
        let s3_fut = self.get_s3_object_contents(hash);
        tokio::pin!(metadata_fut, s3_fut);
        let mut s3_result = None;
        let metadata_result = loop {
            tokio::select! {
                result = &mut metadata_fut => break result,
                result = &mut s3_fut, if s3_result.is_none() => {
                    s3_result = Some(result);
                }
            }
        };

        // If metadata failed, its error is returned here; s3_fut is dropped (canceled) on the
        // early return. Metadata error takes priority over any S3 error.
        let fragment = metadata_result?;

        let s3_contents = match s3_result {
            Some(r) => r?,
            None => s3_fut.await?,
        };

        let payload = self.read_payload(s3_contents, hash, fragment)?;
        Ok((fragment, payload))
    }
}

#[async_trait]
impl ImmutableStoreTrait for AwsImmutableStore {
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsImmutableStore::exists" skip(self))]
    async fn exist(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        let repository: Context = partition.into();
        timed!(self.latency_histogram, &self.labels_exist, {
            if self.exists(repository, address, match_requested).await? {
                Ok(match_requested)
            } else {
                Ok(StoreMatch::MatchNone)
            }
        })
        .into()
    }

    #[lore_macro::lore_instrument]
    async fn exist_batch(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let repository: Context = partition.into();
        timed!(self.latency_histogram, &self.labels_exist_batch, {
            match match_requested {
                StoreMatch::MatchNone => {
                    Ok(addresses.iter().map(|_| StoreMatch::MatchNone).collect())
                }
                StoreMatch::MatchHash | StoreMatch::MatchPartition => {
                    // We cannot use Dynamo batch gets for these, so must fall back to performing
                    // individual prefix queries
                    self.exist_batch_inexact(repository, addresses, match_requested)
                        .await
                }
                StoreMatch::MatchFull => self.exist_batch_exact(repository, addresses).await,
            }
        })
        .into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsImmutableStore::query" skip(self))]
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        let repository: Context = partition.into();
        timed!(self.latency_histogram, &self.labels_query, {
            self.do_query(
                repository,
                address,
                match_requested,
                true, /* hide obliterates */
            )
            .await
        })
        .into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsImmutableStore::get" skip(self))]
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        let repository: Context = partition.into();
        let result: Result<(Fragment, Bytes), StoreError> =
            timed!(self.latency_histogram, &self.labels_get, {
                // Run both futures concurrently. The select! loop breaks as soon as exists resolves.
                // If load finishes first its result is stashed, and we keep waiting for exists check.
                let exists_fut = self.ensure_exists(repository, address, match_required);
                let load_fut = self.load(address.hash);
                tokio::pin!(exists_fut, load_fut);

                let mut load_result = None;
                let exists_result = loop {
                    tokio::select! {
                        result = &mut exists_fut => break result,
                        result = &mut load_fut, if load_result.is_none() => {
                            load_result = Some(result);
                        }
                    }
                };
                // If exists failed, its error is returned here; load_fut is dropped (canceled) on the
                // early return. Exists error takes priority over any load error.
                exists_result?;

                let load_output = match load_result {
                    Some(r) => r?,
                    None => load_fut.await?,
                };

                Ok(load_output)
            })
            .into();
        let (fragment, payload) = result?;
        lore_storage::validate_fragment_payload(&fragment, payload.len())?;
        Ok((fragment, payload))
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsImmutableStore::put" skip(self, fragment, payload))]
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        sanitise_fragment_behavior_flags(&mut fragment);

        if let Some(payload) = payload.as_ref() {
            lore_storage::validate_fragment_payload(&fragment, payload.len())?;
        } else {
            lore_storage::validate_fragment_size(&fragment)?;
        }
        let repository: Context = partition.into();
        timed!(
            self.latency_histogram,
            &self.labels_put,
            {
                let query = self.do_query(
                    repository,
                    address,
                    StoreMatch::MatchFull,
                    false, /* hide obliterates */
                )
                .await;

                let match_made = if !self.force_write && query.is_ok() {
                    let query = query?;

                    if (query.fragment.flags & FragmentFlags::PayloadObliterating) == FragmentFlags::PayloadObliterating
                    {
                        info!("Received request to put fragment at {address} that is in the process of being obliterated");
                        return Err(StoreError::internal(format!("Failed to obliterate immutable {address}")));
                    }

                    if query.match_made != StoreMatch::MatchNone
                        && fragment.size_content != query.fragment.size_content
                        && (query.fragment.flags & FragmentFlags::PayloadObliterated) != FragmentFlags::PayloadObliterated
                    {
                        return Err(StoreError::internal("Hash collision"));
                    }

                    query.match_made
                } else {
                    // If we're in this branch because the query failed, we should log the error.
                    if let Err(e) = query {
                        warn!("Query failed for address: {address:?} in repository: {repository}: {e:?}");
                    }

                    StoreMatch::MatchNone
                };

                match match_made {
                    // If the fragment exists with the same context, there's nothing to do.
                    StoreMatch::MatchFull => Ok(()),

                    // If we matched on hash + repo, then we need to associate the fragment with the new
                    // context. Does not need the payload as it already exist in repository.
                    StoreMatch::MatchPartition => {
                        self.associate_fragment(repository, address).await
                    }

                    // If we were only able to match on hash, the payload must have been provided.
                    // If so, associate the fragment.
                    StoreMatch::MatchHash if payload.is_some() => {
                        self.associate_fragment(repository, address).await
                    }

                    // If no match, the payload must have been provided. Write it to S3 and store fragment.
                    StoreMatch::MatchNone if payload.is_some() => {
                        self.write_payload(repository, address, fragment, payload.unwrap())
                            .await
                    }

                    // If we were only able to match on hash, or were not able to match at all, and no
                    // payload was provided, that's an error.
                    StoreMatch::MatchHash | StoreMatch::MatchNone => {
                        Err(StoreError::internal("Payload buffer required"))
                    }
                }
            }
        )
            .into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name= "AwsImmutableStore::obliterate" skip(self, stats))]
    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let repository: Context = partition.into();
        timed!(self.latency_histogram, &self.labels_obliterate, {
            // Note: given the importance of the work done here, and how relatively infrequently we
            // expect this to be invoked, the log output in this method is intentionally very verbose.
            let span = tracing::Span::current();

            let original_metadata = self
                .metadata_with_size_validation(address.hash)
                .instrument(span.clone())
                .await?;

            info!("Original metadata: {original_metadata:?}");

            // Acquire the lock on the fragment.
            let updated_metadata = if original_metadata.flags & FragmentFlags::PayloadObliteration == 0
            {
                let mut updated_metadata = original_metadata;
                updated_metadata.flags |= FragmentFlags::PayloadObliterating;

                self.update_metadata(address, updated_metadata, original_metadata)
                    .instrument(span.clone())
                    .await?;
                info!("Acquired obliteration lock, updated metadata: {updated_metadata:?}");
                updated_metadata
            } else {
                info!("Fragment metadata indicates fragment is already being (or has previously been) obliterated");
                return Ok(());
            };

            if updated_metadata.flags & FragmentFlags::PayloadFragmented != 0 {
                info!("Fragment is fragmented");
                // There's no reason we couldn't use the `updated_metadata` here, since `read_payload`
                // only cares about the size fields (which haven't changed), but it feels wrong given it
                // doesn't explicitly match the metadata for what's currently in S3.
                let payload = self
                    .read_payload(self.get_s3_object_contents(address.hash).await?, address.hash, original_metadata)?
                    .to_aligned::<FragmentReference>();

                let sub_fragments = payload.as_type_slice::<FragmentReference>();
                info!("Fragment has {} sub-fragments", sub_fragments.len());

                let mut join_set = JoinSet::new();
                for reference in sub_fragments.iter() {
                    let self_clone = self.clone();
                    let stats = stats.clone();
                    let address = Address {
                        hash: reference.hash,
                        context: address.context,
                    };

                    info!("Spawning task to obliterate {address}");
                    lore_base::lore_spawn!(
                        join_set,
                        async move {
                            self_clone
                                .obliterate(repository.into(), address, stats)
                                .await
                                .map_err(|e| (address, e))
                        }
                        .instrument(span.clone())
                    );
                }

                let mut failures = false;
                while let Some(result) = join_set.join_next().await {
                    if let Err(e) = result {
                        failures = true;
                        warn!("Failed to join task for fragment reference obliterate: {e:?}");
                        continue;
                    }

                    // We wouldn't reach this if the result is an `Err`, so this unwrap is guaranteed
                    // not to panic.
                    let result = result.unwrap();
                    if let Err(e) = result {
                        failures = true;
                        warn!("Obliteration failed for sub-fragment {address}: {e:?}");
                    }
                }

                if failures {
                    warn!("Obliteration failed for at least one sub-fragment.");
                    return Err(StoreError::internal(format!("Failed to obliterate immutable {address}")));
                }

                info!("Done obliterating sub-fragments");
            }

            self.delete_association(repository, address)
                .instrument(span.clone())
                .await?;
            stats
                .num_fragments
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // TODO(jcohen): Assuming we always lock the fragment regardless of the association count
            //  then this process of re-checking the count after removing the association should
            //  theoretically not be necessary since no one else should have been able to add a new
            //  fragment association while we maintain the lock.
            info!("Association deleted, re-checking for other association...");
            let remain_associated = self
                .has_associations(address.hash)
                .instrument(span.clone())
                .await?;

            // If the association count is still >= 1 after we deleted, other references remain, so
            // there's nothing left to do...
            if remain_associated {
                info!("Fragment still associated, nothing more to do");
                return self
                    .update_metadata(address, original_metadata, updated_metadata)
                    .instrument(span.clone())
                    .await
                    .inspect_err(|e| {
                        warn!("Failed to reset metadata back to original state: {e:?}");
                    });
            }

            self.delete_payload(address.hash)
                .instrument(span.clone())
                .await?;

            stats
                .num_payloads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let mut obliterated_metadata = updated_metadata;
            obliterated_metadata.flags = FragmentFlags::PayloadObliterated.bits();
            obliterated_metadata.size_payload = 0;
            obliterated_metadata.size_content = 0;

            // Final metadata update to clear out the sizes and set the flags to `Obliterated`.
            self.update_metadata(address, obliterated_metadata, updated_metadata)
                .await
                .inspect_err(|e| {
                    // At this point we've already deleted the underlying payload, so there's not any
                    // point in trying to revert the metadata, that fragment is just well and truly
                    // broken.
                    warn!("Failed to finalize obliterate for {address}: {e:?}");
                })
        }).into()
    }

    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "AwsImmutableStore::copy" skip(self))]
    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        // S3 itself tracks the destination object's existence as the source of durability; the
        // local-flag bookkeeping that `durable` controls is irrelevant here.
        _durable: bool,
    ) -> Result<(), StoreError> {
        let source_repository: Context = source_partition.into();
        let destination_repository: Context = destination_partition.into();
        // The destination tuple shares the source's hash but takes the caller's chosen context
        // — that is the only field the storage trait allows the caller to pivot on a copy.
        let destination_address = Address {
            hash: source_address.hash,
            context: destination_context,
        };
        timed!(self.latency_histogram, &self.labels_copy, {
            let query = self
                .do_query(
                    source_repository,
                    source_address,
                    StoreMatch::MatchFull,
                    false,
                )
                .await?;

            if query.match_made != StoreMatch::MatchFull {
                return Err(StoreError::from(AddressNotFound::from(source_address)));
            }

            self.associate_fragment(destination_repository, destination_address)
                .await
        })
        .into()
    }

    async fn evict(
        self: Arc<Self>,
        _max_capacity: usize,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        // AWS store does not evict anything, ever
        Ok(0)
    }

    async fn compact(
        self: Arc<Self>,
        _max_size: usize,
        _at: Option<usize>,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        // AWS store does not compact anything, ever
        Ok(None)
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        // AWS store does not compact anything, ever
        None
    }

    async fn compact_stop(self: Arc<Self>) {}

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        Ok(())
    }

    fn max_query_batch(&self) -> Option<usize> {
        // DynamoDB batch size cannot exceed 100
        Some(crate::dynamodb::BATCH_GET_ITEM_MAX_COUNT)
    }
}

struct AwsImmutableStoreInstrumentProvider;

impl InstrumentProvider for AwsImmutableStoreInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.store.immutable.aws"
    }

    fn labels(&self) -> &[KeyValue] {
        STORE_ATTRIBUTES.as_slice()
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
    use aws_sdk_dynamodb::operation::delete_item::DeleteItemOutput;
    use aws_sdk_dynamodb::operation::get_item::GetItemError;
    use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
    use aws_sdk_dynamodb::operation::put_item::PutItemOutput;
    use aws_sdk_dynamodb::operation::query::QueryError;
    use aws_sdk_dynamodb::operation::query::QueryOutput;
    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_sdk_dynamodb::types::error::ConditionalCheckFailedException;
    use aws_sdk_dynamodb::types::error::ProvisionedThroughputExceededException;
    use aws_sdk_dynamodb::types::error::ResourceNotFoundException;
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_sdk_s3::operation::delete_object::DeleteObjectError;
    use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
    use aws_sdk_s3::operation::get_object::GetObjectOutput;
    use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsError;
    use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsOutput;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::primitives::SdkBody;
    use aws_sdk_s3::types::ObjectVersion;
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::client::result::SdkError;
    use aws_smithy_runtime_api::client::result::ServiceError;
    use aws_smithy_runtime_api::client::result::TimeoutError;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::FragmentFlags;
    use lore_revision::fragment;
    use lore_storage::ImmutableStore;
    use mockall::predicate::eq;
    use rand::Rng;
    use rand::random;
    use tracing_test::traced_test;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::dynamodb::MockDynamoDb;
    use crate::s3::MockS3Impl;
    use crate::store::address_with_random_context;
    use crate::store::setup_execution;

    const BUCKET: &str = "test-bucket";
    const FRAGMENTS_TABLE_NAME: &str = "fragments";
    const METADATA_TABLE_NAME: &str = "metadata";

    fn mock_lookup_fragments(
        dynamodb_mock: &mut MockDynamoDb,
        fragment_entry: FragmentsEntry,
        starting_match: StoreMatch,
        expected_match: StoreMatch,
    ) {
        let mut store_match = Some(starting_match);

        while store_match.is_some() {
            let m = store_match.unwrap();
            if m == StoreMatch::MatchNone {
                return;
            }

            let matched = m == expected_match;

            match m {
                StoreMatch::MatchFull => {
                    let av_map: HashMap<String, AttributeValue> =
                        serde_dynamo::to_item(fragment_entry.clone()).unwrap();
                    let item = if matched { Some(av_map.clone()) } else { None };

                    dynamodb_mock
                        .expect_get_item()
                        .with(
                            eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)),
                            eq(av_map),
                            eq(true),
                        )
                        .return_once(move |_, _, _| {
                            Ok(GetItemOutput::builder().set_item(item).build())
                        });
                }
                StoreMatch::MatchPartition => {
                    dynamodb_mock
                        .expect_query_single()
                        .with(
                            eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)),
                            eq(FragmentsQuery::Repository(
                                fragment_entry.hash,
                                Context::from(
                                    &fragment_entry.repository_context[..size_of::<Context>()],
                                ),
                            )),
                        )
                        .return_once(move |_, _| {
                            Ok(QueryOutput::builder()
                                .count(if matched { 1 } else { 0 })
                                .build())
                        });
                }
                StoreMatch::MatchHash => {
                    dynamodb_mock
                        .expect_query_single()
                        .with(
                            eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)),
                            eq(FragmentsQuery::Hash(fragment_entry.hash)),
                        )
                        .return_once(move |_, _| {
                            Ok(QueryOutput::builder()
                                .count(if matched { 1 } else { 0 })
                                .build())
                        });
                }
                StoreMatch::MatchNone => unreachable!(),
            }

            if matched {
                break;
            } else {
                store_match = store_match.unwrap().prev();
            }
        }
    }

    fn mock_associate_fragment(dynamodb_mock: &mut MockDynamoDb, entry: &FragmentsEntry) {
        let item: HashMap<String, AttributeValue> = serde_dynamo::to_item(entry).unwrap();

        dynamodb_mock
            .expect_put_item()
            .with(eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)), eq(item.clone()))
            .return_once(move |_, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });
    }

    async fn initialize_immutable_store(s3: S3, dynamodb: DynamoDb) -> AwsImmutableStore {
        let settings = AwsImmutableStoreSettings {
            s3: S3StoreSettings::new(BUCKET.to_string()),
            dynamodb: DynamoDbImmutableStoreSettings::new(
                FRAGMENTS_TABLE_NAME.to_string(),
                METADATA_TABLE_NAME.to_string(),
            ),
            force_write: false,
            batch_exist_submission_limit: 1000,
        };

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                AwsImmutableStore::new(s3, dynamodb, &settings)
            })
            .await
    }

    #[tokio::test]
    async fn test_exists_batch_full_match() {
        let repository = random::<Context>();

        let mut rng = rand::rng();

        #[allow(clippy::type_complexity)]
        let fragments: Vec<(
            FragmentsEntry,
            HashMap<String, AttributeValue>,
            StoreMatch,
            Option<HashMap<String, AttributeValue>>,
        )> = (1..=20)
            .map(|_| {
                let address = random::<Address>();
                let found: bool = rng.random();

                let entry = FragmentsEntry::new(repository, address);
                let av_map: HashMap<String, AttributeValue> =
                    serde_dynamo::to_item(entry.clone()).unwrap();

                let (mock_match, mock_item) = if found {
                    (StoreMatch::MatchFull, Some(av_map.clone()))
                } else {
                    (StoreMatch::MatchNone, None)
                };

                (entry, av_map, mock_match, mock_item)
            })
            .collect();

        let addresses: Vec<Address> = fragments
            .iter()
            .map(|f| Into::<Address>::into(&f.0))
            .collect();
        let items: Vec<HashMap<String, AttributeValue>> =
            fragments.iter().map(|f| f.1.clone()).collect();
        let matches: Vec<StoreMatch> = fragments.iter().map(|f| f.2).collect();
        let response_items: Vec<HashMap<String, AttributeValue>> =
            fragments.iter().filter_map(|f| f.3.clone()).collect();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        dynamodb_mock
            .expect_batch_get_item()
            .with(
                eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)),
                eq(items),
                eq(true),
            )
            .return_once(move |_, _, _| Ok(response_items));

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let result = store
            .clone()
            .exist_batch(
                repository.into(),
                addresses.as_slice(),
                StoreMatch::MatchFull,
            )
            .await
            .expect("exist batch failed");

        assert_eq!(matches, result);
    }

    #[tokio::test]
    async fn test_query_immutable_not_found() {
        let repository = random::<Context>();
        let address = random::<Address>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(repository, address),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let result = store
            .clone()
            .query(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("query immutable failed");

        assert_eq!(
            StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone
            },
            result
        );
    }

    #[tokio::test]
    async fn test_query_immutable_found() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(repository, address),
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let result = store
            .clone()
            .query(repository.into(), address, StoreMatch::MatchFull)
            .await
            .unwrap();

        assert_eq!(
            StoreQueryResult {
                fragment,
                match_made: StoreMatch::MatchFull
            },
            result
        );
    }

    #[tokio::test]
    async fn test_query_immutable_obliterating() {
        let repository = random::<Context>();
        let (mut fragment, address, _) = fragment::generate_random();
        fragment.flags |= FragmentFlags::PayloadStoredDurable | FragmentFlags::PayloadObliterating;

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(repository, address),
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let result = store
            .clone()
            .query(repository.into(), address, StoreMatch::MatchFull)
            .await
            .unwrap();

        assert_eq!(
            StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone
            },
            result
        );
    }

    #[tokio::test]
    async fn test_query_immutable_partial_match() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(repository, address),
            StoreMatch::MatchPartition,
            StoreMatch::MatchPartition,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let other_address = address_with_random_context(address);

        let result = store
            .clone()
            .query(repository.into(), other_address, StoreMatch::MatchPartition)
            .await
            .unwrap();

        assert_eq!(
            StoreQueryResult {
                fragment,
                match_made: StoreMatch::MatchPartition
            },
            result
        );
    }

    #[tokio::test]
    async fn test_query_lower_specificity_match() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let other_address = address_with_random_context(address);

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(repository, other_address),
            StoreMatch::MatchPartition,
            StoreMatch::MatchHash,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let result = store
            .clone()
            .query(repository.into(), other_address, StoreMatch::MatchPartition)
            .await
            .unwrap();

        assert_eq!(
            StoreQueryResult {
                fragment,
                match_made: StoreMatch::MatchHash
            },
            result
        );
    }

    #[tokio::test]
    async fn test_put_immutable() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(address.hash).with_fragment(fragment))
                .unwrap();

        dynamodb_mock
            .expect_put_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(item.clone()))
            .return_once(move |_, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        s3mock
            .expect_put_object()
            .with(
                eq(BUCKET),
                eq(address.hash.to_string()),
                eq(payload.to_vec()),
            )
            .return_once(move |_, _, _: Vec<u8>| Ok(PutObjectOutput::builder().build()));

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("failed to write to store");
    }

    #[tokio::test]
    #[ignore] // Partial puts are not currently supported
    async fn test_put_immutable_partial() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchPartition,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("failed to write to store");
    }

    #[tokio::test]
    async fn test_put_immutable_obliterating() {
        let repository = random::<Context>();
        let (mut fragment, address, payload) = fragment::generate_random();
        fragment.flags = FragmentFlags::PayloadObliterating.bits();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .expect_err("expected put to fail")
                .is_internal()
        );
    }

    #[tokio::test]
    async fn test_put_immutable_obliterated() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchHash,
        );

        let obliterated_fragment = Fragment {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 0,
            size_content: 0,
        };

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(obliterated_fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(address.hash).with_fragment(fragment))
                .unwrap();

        dynamodb_mock
            .expect_put_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(item.clone()))
            .return_once(move |_, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        s3mock
            .expect_put_object()
            .with(
                eq(BUCKET),
                eq(address.hash.to_string()),
                eq(payload.to_vec()),
            )
            .return_once(move |_, _, _: Vec<u8>| Ok(PutObjectOutput::builder().build()));

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        store
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("failed to write to store");
    }

    #[tokio::test]
    #[ignore] // Partial puts are not currently supported
    async fn test_put_immutable_partial_hash_collision() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchPartition,
        );

        let mut different_fragment = fragment;
        different_fragment.size_content *= 2;

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(different_fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .err()
                .unwrap()
                .is_internal()
        );
    }

    #[tokio::test]
    async fn test_put_immutable_payload_required() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchHash,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry = metadata_entry.with_fragment(fragment);
        let full_entry_av_map = serde_dynamo::to_item(full_entry.clone()).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .put(repository.into(), address, fragment, None, false)
                .await
                .expect_err("should have returned an error")
                .is_internal()
        );
    }

    #[tokio::test]
    async fn test_put_immutable_extra_data() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(address.hash).with_fragment(fragment))
                .unwrap();

        dynamodb_mock
            .expect_put_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(item.clone()))
            .return_once(move |_, _| {
                Ok(PutItemOutput::builder().set_attributes(Some(item)).build())
            });

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let mut body = vec![];
        body.extend_from_slice(payload.as_ref());

        let real_len = body.len();

        let extra = random::<[u8; 32]>();
        body.extend_from_slice(extra.as_slice());

        // Ensure we only write bytes equal to the actual payload size, regardless of how much extra
        // was sent.
        let expected = body[..real_len].to_vec();
        s3mock
            .expect_put_object()
            .with(eq(BUCKET), eq(address.hash.to_string()), eq(expected))
            .return_once(move |_, _, _: Vec<u8>| Ok(PutObjectOutput::builder().build()));

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        store
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("failed to write to store");
    }

    #[tokio::test]
    async fn test_put_immutable_not_enough_data() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let mut body = vec![];
        body.extend_from_slice(fragment.as_bytes());

        let truncated_payload = Bytes::copy_from_slice(&payload[..payload.len() - 1]);

        body.extend_from_slice(truncated_payload.as_ref());

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(truncated_payload),
                    false
                )
                .await
                .expect_err("should have failed")
                .is_internal()
        );
    }

    #[tokio::test]
    async fn test_get_immutable() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchHash,
            StoreMatch::MatchHash,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let mut s3payload = vec![];
        s3payload.extend_from_slice(payload.as_ref());

        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(address.hash.to_string()), eq(None))
            .return_once(move |_, _, _| {
                Ok(GetObjectOutput::builder()
                    .set_body(Some(s3payload.into()))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let (result_fragment, result_buffer) = store
            .get(repository.into(), address, StoreMatch::MatchHash)
            .await
            .expect("failed to get from store");

        assert_eq!(fragment, result_fragment);

        assert_eq!(payload.as_ref(), result_buffer.as_ref());
    }

    #[tokio::test]
    async fn test_get_immutable_not_found() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchHash,
            StoreMatch::MatchNone,
        );

        // `load` runs concurrently with `ensure_exists` in `get`, and its two internal
        // futures (`load_metadata` and `get_s3_object_contents`) also race each other.
        // Depending on select! polling order either or both may be called before being
        // cancelled by the `ensure_exists` error, so these expectations are optional.
        {
            let metadata_entry = FragmentMetadataEntry::new(address.hash);
            let av_map: HashMap<String, AttributeValue> =
                serde_dynamo::to_item(metadata_entry.clone()).unwrap();
            let full_entry_av_map =
                serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();
            dynamodb_mock
                .expect_get_item()
                .with(
                    eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                    eq(av_map),
                    eq(true),
                )
                .times(..=1)
                .return_once(move |_, _, _| {
                    Ok(GetItemOutput::builder()
                        .set_item(Some(full_entry_av_map))
                        .build())
                });

            let mut s3payload = vec![];
            s3payload.extend_from_slice(payload.as_ref());
            s3mock
                .expect_get_object()
                .with(eq(BUCKET), eq(address.hash.to_string()), eq(None))
                .times(..=1)
                .return_once(move |_, _, _| {
                    Ok(GetObjectOutput::builder()
                        .set_body(Some(s3payload.into()))
                        .build())
                });
        }

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .get(repository.into(), address, StoreMatch::MatchHash,)
                .await
                .expect_err("should have returned an error")
                .is_address_not_found()
        );
    }

    #[tokio::test]
    async fn test_get_immutable_obliterated() {
        let (_, address, payload) = fragment::generate_random();
        let repository = random::<Context>();
        let fragment = Fragment {
            flags: FragmentFlags::PayloadObliterating.bits(),
            size_payload: 0,
            size_content: 0,
        };

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `get` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchHash,
            StoreMatch::MatchHash,
        );

        // the store will opportunistically try to get the data
        // from s3, but because the metadata shows it is obliterated
        // it will not load, even if s3 says it is there
        {
            let mut s3payload = vec![];
            s3payload.extend_from_slice(payload.as_ref());

            s3mock.expect_get_object().return_once(|_, _, _| {
                Ok(GetObjectOutput::builder()
                    .set_body(Some(s3payload.into()))
                    .build())
            });
        }

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let err = store
            .get(repository.into(), address, StoreMatch::MatchHash)
            .await
            .expect_err("should have returned an error");

        assert!(err.is_address_not_found());
    }

    #[allow(dead_code)]
    async fn test_get_immutable_partial_match() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = DynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchPartition,
        );

        let mut s3payload = vec![];
        s3payload.extend_from_slice(fragment.as_bytes());
        s3payload.extend_from_slice(payload.as_ref());

        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(address.hash.to_string()), eq(None))
            .return_once(move |_, _, _| {
                Ok(GetObjectOutput::builder()
                    .set_body(Some(s3payload.into()))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let (result_fragment, result_buffer) = store
            .get(repository.into(), address, StoreMatch::MatchPartition)
            .await
            .expect("failed to get from store");

        assert_eq!(fragment, result_fragment);

        assert_eq!(payload.as_ref(), result_buffer.as_ref());
    }

    #[tokio::test]
    async fn test_get_immutable_payload_size_mismatch() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // Mock the list objects calls that `put_immutable` makes when querying for an object.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchHash,
            StoreMatch::MatchHash,
        );

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        let mut s3payload = vec![];
        s3payload.extend_from_slice(&payload.as_ref()[..16]);

        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(address.hash.to_string()), eq(None))
            .return_once(move |_, _, _| {
                Ok(GetObjectOutput::builder()
                    .set_body(Some(s3payload.into()))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        assert!(
            store
                .get(repository.into(), address, StoreMatch::MatchHash,)
                .await
                .expect_err("Request did not fail as expected")
                .is_internal()
        );
    }

    fn mock_load_fragment_metadata(
        dynamodb_mock: &mut MockDynamoDb,
        extra_flags: Option<FragmentFlags>,
        fail: bool,
    ) -> (Fragment, Address) {
        let (mut fragment, address, _payload) = fragment::generate_random();

        fragment.flags |= FragmentFlags::PayloadStoredDurable;
        if let Some(extra_flags) = extra_flags {
            fragment.flags |= extra_flags;
        }

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        // Mock loading the fragment metadata
        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                if fail {
                    Ok(GetItemOutput::builder().set_item(None).build())
                } else {
                    Ok(GetItemOutput::builder()
                        .set_item(Some(full_entry_av_map))
                        .build())
                }
            });

        (fragment, address)
    }

    #[tokio::test]
    async fn test_obliterate_already_obliterating() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (_fragment, address) = mock_load_fragment_metadata(
            &mut dynamodb_mock,
            Some(FragmentFlags::PayloadObliterating),
            false, /* fail */
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats = Default::default();
        Arc::new(store)
            .obliterate(repository.into(), address, stats)
            .await
            .expect("obliterate failed");
    }

    #[tokio::test]
    async fn test_obliterate_already_obliterated() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (_fragment, address) = mock_load_fragment_metadata(
            &mut dynamodb_mock,
            Some(FragmentFlags::PayloadObliterated),
            false, /* fail */
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats = Default::default();
        Arc::new(store)
            .obliterate(repository.into(), address, stats)
            .await
            .expect("obliterate failed");
    }

    #[derive(Clone, Copy)]
    enum MockLockMode {
        Finalize,
        Revert,
        AcquireFail,
        FinalizeFail,
        None,
    }

    fn aws_error<E>(error: E, status: u16) -> AwsError<SdkError<E, HttpResponse>> {
        AwsError::AwsSdkError(SdkError::ServiceError(
            ServiceError::builder()
                .source(error)
                .raw(HttpResponse::new(
                    status.try_into().unwrap(),
                    SdkBody::empty(),
                ))
                .build(),
        ))
    }

    fn mock_acquire_obliterate_lock(
        dynamodb_mock: &mut MockDynamoDb,
        fragment: Fragment,
        hash: Hash,
        lock_mode: MockLockMode,
        in_sequence: bool,
    ) {
        let mut updated_metadata = fragment;
        updated_metadata.flags |= FragmentFlags::PayloadObliterating;
        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(hash).with_fragment(updated_metadata))
                .expect("failed to serialize");

        let mut seq = mockall::Sequence::default();

        // Mock the metadata updates to acquire the lock
        let mut expectation = dynamodb_mock.expect_put_item_conditional().times(1);

        if in_sequence {
            expectation = expectation.in_sequence(&mut seq);
        }

        expectation
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(item.clone()),
                eq(UpdateMetadataCondition(fragment)),
            )
            .return_once(move |_, _, _| {
                if matches!(lock_mode, MockLockMode::AcquireFail) {
                    Err(aws_error(
                        PutItemError::ConditionalCheckFailedException(
                            ConditionalCheckFailedException::builder()
                                .set_item(Some(item))
                                .build(),
                        ),
                        400u16,
                    ))
                } else {
                    Ok(PutItemOutput::builder().build())
                }
            });

        match lock_mode {
            MockLockMode::Finalize | MockLockMode::FinalizeFail => {
                let mut final_metadata = updated_metadata;
                final_metadata.flags = FragmentFlags::PayloadObliterated.bits();
                final_metadata.size_content = 0;
                final_metadata.size_payload = 0;
                let item: HashMap<String, AttributeValue> = serde_dynamo::to_item(
                    FragmentMetadataEntry::new(hash).with_fragment(final_metadata),
                )
                .expect("failed to serialize");

                // And a second one that releases the lock
                let mut expectation = dynamodb_mock.expect_put_item_conditional().times(1);

                if in_sequence {
                    expectation = expectation.in_sequence(&mut seq);
                }

                expectation
                    .with(
                        eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                        eq(item.clone()),
                        eq(UpdateMetadataCondition(updated_metadata)),
                    )
                    .return_once(move |_, _, _| {
                        if matches!(lock_mode, MockLockMode::Finalize) {
                            Ok(PutItemOutput::builder().build())
                        } else {
                            Err(aws_error(
                                PutItemError::ConditionalCheckFailedException(
                                    ConditionalCheckFailedException::builder()
                                        .set_item(Some(item))
                                        .build(),
                                ),
                                400u16,
                            ))
                        }
                    });
            }
            MockLockMode::Revert => {
                let item: HashMap<String, AttributeValue> =
                    serde_dynamo::to_item(FragmentMetadataEntry::new(hash).with_fragment(fragment))
                        .expect("failed to serialize");

                // And a second one that releases the lock
                let mut expectation = dynamodb_mock.expect_put_item_conditional().times(1);

                if in_sequence {
                    expectation = expectation.in_sequence(&mut seq);
                }

                expectation
                    .with(
                        eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                        eq(item),
                        eq(UpdateMetadataCondition(updated_metadata)),
                    )
                    .return_once(move |_, _, _| Ok(PutItemOutput::builder().build()));
            }
            MockLockMode::None | MockLockMode::AcquireFail => {}
        }
    }

    fn mock_count_associations(
        dynamodb_mock: &mut MockDynamoDb,
        hash: Hash,
        count: i32,
        fail: bool,
    ) {
        dynamodb_mock
            .expect_query_single()
            .times(1)
            .with(
                eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)),
                eq(FragmentsQuery::HashCount(hash)),
            )
            .return_once(move |_, _| {
                if fail {
                    Err(aws_error(
                        QueryError::ProvisionedThroughputExceededException(
                            ProvisionedThroughputExceededException::builder().build(),
                        ),
                        503u16,
                    ))
                } else {
                    Ok(QueryOutput::builder().count(count).build())
                }
            });
    }

    fn mock_remove_association(
        dynamodb_mock: &mut MockDynamoDb,
        repository: Context,
        address: Address,
        fail: bool,
    ) {
        let entry = FragmentsEntry::new(repository, address);
        let item: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(entry).expect("failed to serialize fragments entry");

        dynamodb_mock
            .expect_delete_item()
            .with(eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)), eq(item))
            .return_once(move |_, _| {
                if fail {
                    Err(aws_error(
                        DeleteItemError::ProvisionedThroughputExceededException(
                            ProvisionedThroughputExceededException::builder().build(),
                        ),
                        503u16,
                    ))
                } else {
                    Ok(DeleteItemOutput::builder().build())
                }
            });
    }

    fn mock_list_versions(
        s3mock: &mut MockS3Impl,
        hash: Hash,
        version: Option<String>,
        fail: bool,
    ) {
        s3mock
            .expect_list_versions()
            .with(eq(BUCKET), eq(hash.to_string()))
            .return_once(move |_, _| {
                if fail {
                    Err(aws_error(
                        ListObjectVersionsError::generic(ErrorMetadata::builder().build()),
                        500u16,
                    ))
                } else {
                    let versions = if version.is_some() {
                        Some(vec![
                            ObjectVersion::builder().set_version_id(version).build(),
                        ])
                    } else {
                        None
                    };
                    Ok(ListObjectVersionsOutput::builder()
                        .set_versions(versions)
                        .build())
                }
            });
    }

    fn mock_delete_payload(
        s3mock: &mut MockS3Impl,
        hash: Hash,
        version: Option<String>,
        fail: bool,
    ) {
        s3mock
            .expect_delete_object()
            .with(eq(BUCKET), eq(hash.to_string()), eq(version))
            .return_once(move |_, _, _| {
                if fail {
                    Err(aws_error(
                        DeleteObjectError::generic(ErrorMetadata::builder().build()),
                        500u16,
                    ))
                } else {
                    Ok(DeleteObjectOutput::builder().build())
                }
            });
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::Finalize,
            true, /* in sequence */
        );

        // Mock the association count, this is currently done twice (for now), the first time we
        // return 1, the second 0.
        mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );

        let version_id = Some("some-version".to_string());
        mock_list_versions(&mut s3mock, address.hash, version_id.clone(), false);

        mock_delete_payload(&mut s3mock, address.hash, version_id, false /* fail */);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        Arc::new(store)
            .obliterate(repository.into(), address, stats.clone())
            .await
            .expect("obliterate failed");

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_multiple_associations() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::Revert,
            true, /* in sequence */
        );

        // Mock the association count, this is currently done twice (for now), the first time we
        // return 2, the second 1.
        mock_count_associations(&mut dynamodb_mock, address.hash, 1, false /* fail */);

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        Arc::new(store)
            .obliterate(repository.into(), address, stats.clone())
            .await
            .expect("obliterate failed");

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_metadata_load_fails() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (_fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, true /* fail */);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_address_not_found()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 0);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_load_metadata_sdk_timeout_returns_slow_down() {
        let (_fragment, address, _payload) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry).unwrap();

        #[derive(Debug, thiserror::Error)]
        #[error("stub")]
        struct StubError;

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Err(AwsError::AwsSdkError(SdkError::TimeoutError(
                    TimeoutError::builder().source(Box::new(StubError)).build(),
                )))
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        assert!(
            store
                .load_metadata(address.hash)
                .await
                .unwrap_err()
                .is_slow_down()
        );
    }

    #[tokio::test]
    async fn test_load_metadata_sdk_service_error_returns_address_not_found() {
        let (_fragment, address, _payload) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Err(aws_error(
                    GetItemError::ResourceNotFoundException(
                        ResourceNotFoundException::builder()
                            .message("Table not found")
                            .build(),
                    ),
                    400u16,
                ))
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        assert!(
            store
                .load_metadata(address.hash)
                .await
                .unwrap_err()
                .is_address_not_found()
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_acquire_lock_fails() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::AcquireFail,
            true, /* in sequence */
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_internal(),
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 0);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_count_associations_fails() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::None,
            true, /* in sequence */
        );

        mock_remove_association(&mut dynamodb_mock, repository, address, false);

        mock_count_associations(&mut dynamodb_mock, address.hash, 0, true /* fail */);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_slow_down()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_remove_fragment_association_fails() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::None,
            true, /* in sequence */
        );

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            true, /* fail */
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_slow_down()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 0);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    // Delete payload fails
    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_delete_payload_fails() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::None,
            true, /* in sequence */
        );

        // Mock the association count, this is currently done twice (for now), the first time we
        // return 1, the second 0.
        mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );

        let version_id = Some("some-version".to_string());
        mock_list_versions(&mut s3mock, address.hash, version_id.clone(), false);

        mock_delete_payload(&mut s3mock, address.hash, version_id, true /* fail */);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_slow_down()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_delete_payload_fails_to_list_versions() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::None,
            true, /* in sequence */
        );

        // Mock the association count, this is currently done twice (for now), the first time we
        // return 1, the second 0.
        mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );

        let version_id = Some("some-version".to_string());
        mock_list_versions(&mut s3mock, address.hash, version_id.clone(), true);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_slow_down()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
    }

    // Finalize metadata fails
    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_single_fragment_finalize_metadata_fails() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::FinalizeFail,
            true, /* in sequence */
        );

        // Mock the association count, this is currently done twice (for now), the first time we
        // return 1, the second 0.
        mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );

        let version_id = Some("some-version".to_string());
        mock_list_versions(&mut s3mock, address.hash, version_id.clone(), false);

        mock_delete_payload(&mut s3mock, address.hash, version_id, false /* fail */);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_internal()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_fragment_is_fragmented() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        // Build the fragment list payload
        let address = random::<Address>();
        let context = address.context;

        let mut payload = BytesMut::new();
        const SUB_FRAGMENT_COUNT: u64 = 5;
        const SUB_FRAGMENT_SIZE: u64 = 32;

        for i in 0..SUB_FRAGMENT_COUNT {
            let (fragment, mut address) =
                mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);
            address.context = context;

            mock_acquire_obliterate_lock(
                &mut dynamodb_mock,
                fragment,
                address.hash,
                MockLockMode::Finalize,
                // We do not mock the expectations in sequence because order of obliterates for each
                // sub-fragment is non-deterministic.
                false, /* in sequence */
            );

            // Mock the association count, this is currently done twice (for now), the first time we
            // return 1, the second 0.
            mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

            mock_remove_association(
                &mut dynamodb_mock,
                repository,
                address,
                false, /* fail */
            );

            let version_id = Some("some-version".to_string());
            mock_list_versions(&mut s3mock, address.hash, version_id.clone(), false);

            mock_delete_payload(&mut s3mock, address.hash, version_id, false /* fail */);

            let reference = FragmentReference {
                hash: address.hash,
                offset_content: i * SUB_FRAGMENT_SIZE,
            };
            payload.extend_from_slice(reference.as_bytes());
        }

        let fragment = Fragment {
            flags: (FragmentFlags::PayloadStoredDurable | FragmentFlags::PayloadFragmented).bits(),
            size_payload: payload.len() as u32,
            size_content: SUB_FRAGMENT_SIZE * SUB_FRAGMENT_COUNT,
        };

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        // Mock loading the fragment metadata
        dynamodb_mock
            .expect_get_item()
            .times(1)
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        // Mock reading the payload to get the sub-fragments
        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(format!("{}", address.hash)), eq(None))
            .return_once(move |_, _, _| {
                Ok(GetObjectOutput::builder()
                    .body(payload.to_vec().into())
                    .build())
            });

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::Finalize,
            // We do not mock the expectations in sequence because order of obliterates for each
            // sub-fragment is non-deterministic.
            false, /* in sequence */
        );

        // Mock the association count, this is currently done twice (for now), the first time we
        // return 1, the second 0.
        mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );

        let version_id = Some("some-version".to_string());
        mock_list_versions(&mut s3mock, address.hash, version_id.clone(), false);

        mock_delete_payload(&mut s3mock, address.hash, version_id, false /* fail */);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        Arc::new(store)
            .obliterate(repository.into(), address, stats.clone())
            .await
            .expect("obliterate failed");

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(
            stats.num_fragments.load(Ordering::Relaxed),
            (SUB_FRAGMENT_COUNT + 1) as usize
        );
        assert_eq!(
            stats.num_payloads.load(Ordering::Relaxed),
            (SUB_FRAGMENT_COUNT + 1) as usize
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_obliterate_fragment_is_fragmented_obliterate_subfragment_fails() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        // Build the fragment list payload
        let address = random::<Address>();
        let context = address.context;

        let mut payload = BytesMut::new();
        const SUB_FRAGMENT_COUNT: u64 = 2;
        const SUB_FRAGMENT_SIZE: u64 = 32;

        for i in 0..SUB_FRAGMENT_COUNT {
            let (fragment, mut address) =
                mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);
            address.context = context;

            mock_acquire_obliterate_lock(
                &mut dynamodb_mock,
                fragment,
                address.hash,
                if i == 0 {
                    MockLockMode::Finalize
                } else {
                    MockLockMode::None
                },
                // We do not mock the expectations in sequence because order of obliterates for each
                // sub-fragment is non-deterministic.
                false, /* in sequence */
            );

            // Mock the association count, this is currently done twice (for now), the first time we
            // return 1, the second 0.
            mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

            mock_remove_association(
                &mut dynamodb_mock,
                repository,
                address,
                false, /* fail */
            );

            let version_id = Some("some-version".to_string());
            mock_list_versions(&mut s3mock, address.hash, version_id.clone(), false);

            mock_delete_payload(
                &mut s3mock,
                address.hash,
                version_id,
                i == 1, /* fail for the second sub-fragment */
            );

            let reference = FragmentReference {
                hash: address.hash,
                offset_content: i * SUB_FRAGMENT_SIZE,
            };
            payload.extend_from_slice(reference.as_bytes());
        }

        let fragment = Fragment {
            flags: (FragmentFlags::PayloadStoredDurable | FragmentFlags::PayloadFragmented).bits(),
            size_payload: payload.len() as u32,
            size_content: SUB_FRAGMENT_SIZE * SUB_FRAGMENT_COUNT,
        };

        let metadata_entry = FragmentMetadataEntry::new(address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        // Mock loading the fragment metadata
        dynamodb_mock
            .expect_get_item()
            .times(1)
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        // Mock reading the payload to get the sub-fragments
        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(format!("{}", address.hash)), eq(None))
            .return_once(move |_, _, _| {
                Ok(GetObjectOutput::builder()
                    .body(payload.to_vec().into())
                    .build())
            });

        mock_acquire_obliterate_lock(
            &mut dynamodb_mock,
            fragment,
            address.hash,
            MockLockMode::None,
            // We do not mock the expectations in sequence because order of obliterates for each
            // sub-fragment is non-deterministic.
            false, /* in sequence */
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        assert!(
            Arc::new(store)
                .obliterate(repository.into(), address, stats.clone())
                .await
                .unwrap_err()
                .is_internal()
        );

        // The rest of the necessary assertions are handled by expectations on the Dynamo and S3
        // mocks.
        assert_eq!(
            stats.num_fragments.load(Ordering::Relaxed),
            // We deleted associations for both sub-fragments, but not the parent fragment
            SUB_FRAGMENT_COUNT as usize
        );
        assert_eq!(
            stats.num_payloads.load(Ordering::Relaxed),
            // We deleted payloads for one sub-fragment, but failed on the second which should
            // prevent the parent payload from being deleted as well
            (SUB_FRAGMENT_COUNT - 1) as usize
        );
    }

    #[tokio::test]
    async fn test_copy_not_found() {
        let source_repository = random::<Context>();
        let source_address = random::<Address>();
        let destination_repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        // Source does not exist — lookup returns MatchNone.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(source_repository, source_address),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let err = store
            .copy(
                source_repository.into(),
                source_address,
                destination_repository.into(),
                source_address.context,
                false,
            )
            .await
            .expect_err("copy should have returned NotFound");

        assert!(
            err.is_address_not_found(),
            "Expected AddressNotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_copy_partial_match_returns_not_found() {
        let source_repository = random::<Context>();
        let source_address = random::<Address>();
        let destination_repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        // Fragment exists by hash globally but not in source_repository (MatchHash).
        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(source_repository, source_address),
            StoreMatch::MatchFull,
            StoreMatch::MatchHash,
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        let err = store
            .copy(
                source_repository.into(),
                source_address,
                destination_repository.into(),
                source_address.context,
                false,
            )
            .await
            .expect_err("copy should have returned NotFound for partial match");

        assert!(
            err.is_address_not_found(),
            "Expected AddressNotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_copy_success() {
        let source_repository = random::<Context>();
        let (fragment, source_address, _) = fragment::generate_random();
        let destination_repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        // Source exists at MatchFull.
        mock_lookup_fragments(
            &mut dynamodb_mock,
            FragmentsEntry::new(source_repository, source_address),
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );

        // Metadata load required by do_query when match_made != MatchNone.
        let metadata_entry = FragmentMetadataEntry::new(source_address.hash);
        let av_map: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(metadata_entry.clone()).unwrap();
        let full_entry_av_map =
            serde_dynamo::to_item(metadata_entry.with_fragment(fragment)).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(METADATA_TABLE_NAME)),
                eq(av_map),
                eq(true),
            )
            .return_once(move |_, _, _| {
                Ok(GetItemOutput::builder()
                    .set_item(Some(full_entry_av_map))
                    .build())
            });

        // The destination association should be written to DynamoDB.
        let destination_entry = FragmentsEntry::new(destination_repository, source_address);
        mock_associate_fragment(&mut dynamodb_mock, &destination_entry);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;
        let store = Arc::new(store);

        store
            .copy(
                source_repository.into(),
                source_address,
                destination_repository.into(),
                source_address.context,
                false,
            )
            .await
            .expect("copy should succeed");
    }
}
