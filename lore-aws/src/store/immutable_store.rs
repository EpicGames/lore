// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::string::ToString;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::Select;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
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
use opentelemetry::metrics::Counter;
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
use crate::store::object_metadata::ObjectMetadataError;
use crate::store::object_metadata::from_object_metadata;
use crate::store::object_metadata::to_object_metadata;

pub mod metadata_migrator;

enum QueryResultSource {
    LegacyMetadata(Fragment),
    State,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FragmentsEntry {
    pub(crate) hash: Hash,
    #[serde(with = "serde_bytes")]
    pub(crate) repository_context: [u8; size_of::<Context>() * 2],
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
    pub(crate) fn new(repository: Context, address: Address) -> Self {
        let mut repository_context = [0u8; size_of::<Context>() * 2];
        repository_context[..size_of::<Context>()].copy_from_slice(repository.data());
        repository_context[size_of::<Context>()..].copy_from_slice(address.context.data());

        Self {
            hash: address.hash,
            repository_context,
        }
    }
}

/// Where a payload is in its lifecycle.
///
/// This is the whole of what `DynamoDB` records about a payload. What the payload *is* — its
/// compression, its sizes — lives on the S3 object itself and is never duplicated here, so the two
/// cannot disagree. What `DynamoDB` adds is the ability to answer "does this hash exist, and may it
/// be read" without an S3 request, which is the only reason the row exists at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FragmentState {
    /// The payload is stored and readable.
    Stored,
    /// An obliteration holds this hash. Transient: it is either cleared or advanced to
    /// [`FragmentState::Obliterated`].
    Obliterating,
    /// The payload has been obliterated and its object deleted. A tombstone, kept so the
    /// difference between "never stored" and "deliberately destroyed" survives.
    Obliterated,
}

impl FragmentState {
    fn from_bits(bits: u32) -> Self {
        if bits & FragmentFlags::PayloadObliterated == FragmentFlags::PayloadObliterated {
            Self::Obliterated
        } else if bits & FragmentFlags::PayloadObliterating == FragmentFlags::PayloadObliterating {
            Self::Obliterating
        } else {
            Self::Stored
        }
    }

    fn bits(self) -> u32 {
        match self {
            Self::Stored => 0,
            Self::Obliterating => FragmentFlags::PayloadObliterating.bits(),
            Self::Obliterated => FragmentFlags::PayloadObliterated.bits(),
        }
    }

    fn is_obliteration(self) -> bool {
        self != Self::Stored
    }
}

/// A row in the fragment state table. Presence of the row means the hash exists in some state.
///
/// The `state` field is what distinguishes a row written under this model from one written when
/// fragments were stored in `DynamoDB`: those carry flattened `flags`/`size_payload`/`size_content`
/// instead. A migration can tell the two apart by shape alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FragmentStateEntry {
    hash: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<u32>,
}

/// A row in the shape written before fragments moved onto the S3 object: the whole fragment,
/// flattened alongside the hash.
///
/// Deserialization only. Nothing writes this shape any more.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FragmentMetadataEntry {
    #[allow(dead_code)]
    pub(crate) hash: Hash,
    #[serde(flatten)]
    pub(crate) fragment: Option<Fragment>,
}

impl FragmentStateEntry {
    fn key(hash: Hash) -> Self {
        Self { hash, state: None }
    }

    pub(crate) fn new(hash: Hash, state: FragmentState) -> Self {
        Self {
            hash,
            state: Some(state.bits()),
        }
    }

    pub(crate) fn state(&self) -> FragmentState {
        FragmentState::from_bits(self.state.unwrap_or_default())
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
    pub fragment_state_table_name: String,
    /// Table holding fragments written before they moved onto the S3 object, read only when an
    /// object turns out to carry no metadata of its own.
    ///
    /// Set this on a deployment that has stored objects the old way — normally to the same table as
    /// `fragment_state_table_name`, since both row shapes share it and are told apart by shape. Leaving it
    /// unset declares that no such object exists, which makes an object carrying no metadata what it
    /// then is: damaged, rather than merely old.
    #[serde(default)]
    pub fragment_metadata_table_name: Option<String>,
    pub endpoint_url: Option<String>,
    pub region: Option<String>,
    pub slow_operation_threshold_millis: u64,
    #[serde(default = "default_aws_timeout_millis")]
    pub timeout_millis: u64,
}

impl DynamoDbImmutableStoreSettings {
    pub fn new(fragments_table_name: String, fragment_state_table_name: String) -> Self {
        Self {
            fragments_table_name,
            fragment_state_table_name,
            fragment_metadata_table_name: None,
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

    /// Read fragments for objects predating the move onto the S3 object from `table_name`.
    pub fn with_fragment_metadata_table(mut self, table_name: String) -> Self {
        self.fragment_metadata_table_name = Some(table_name);
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
pub(crate) enum FragmentsQuery {
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

/// Write only if no row exists for this hash yet.
///
/// Publishing a payload uses this so that a concurrent obliteration's mark cannot be erased by a
/// racing writer: the writer's create loses, it re-reads the row, and it sees the mark.
#[derive(Debug, PartialEq)]
pub(crate) struct RowAbsent;

impl DynamoDbPutCondition for RowAbsent {
    fn into_parts(self) -> ConditionParts {
        ConditionParts {
            condition_expression: "attribute_not_exists(#hash)".to_string(),
            expression_names: HashMap::from([("#hash".to_string(), "hash".to_string())]),
            expression_values: HashMap::new(),
        }
    }
}

/// Write only if the row is still in the state the caller last observed.
///
/// Obliteration advances the row through its states with this, so two obliterations racing for the
/// same hash cannot both believe they hold the mark.
#[derive(Debug, PartialEq)]
pub(crate) struct StateUnchanged(pub(crate) FragmentState);

impl DynamoDbPutCondition for StateUnchanged {
    fn into_parts(self) -> ConditionParts {
        ConditionParts {
            condition_expression: "#state = :state".to_string(),
            expression_names: HashMap::from([("#state".to_string(), "state".to_string())]),
            expression_values: HashMap::from([(
                ":state".to_string(),
                AttributeValue::N(self.0.bits().to_string()),
            )]),
        }
    }
}

/// Counts reads that found a partition still referencing a hash whose payload S3 no longer has.
///
/// Non-zero means content has been lost. The read itself is reported as a plain not-found, which is
/// indistinguishable from content that was never stored, so without this the loss is silent.
const METRICS_MISSING_PAYLOAD_METRIC_NAME: &str = "store.immutable.missing_payload";

/// Lower bound on the obliteration drain, regardless of how the `DynamoDB` timeout is configured.
const MIN_OBLITERATION_DRAIN_MILLIS: u64 = 100;

/// Whether a `DynamoDB` failure means "ask again" rather than "here is your answer".
///
/// The SDK signals overload in several shapes — a client-side timeout, a dispatch failure before the
/// request reached the service, an HTTP 429 or 5xx, or a service error whose code names throttling —
/// and they all mean the same thing to a caller: no answer was obtained. Everything else is a real
/// failure and is reported as one.
///
/// Getting this wrong is not cosmetic. Reporting a failed read as not-found tells a caller the
/// content is absent when we merely failed to look, and a not-found on a referenced hash is what
/// [`AwsImmutableStore::report_missing_payload`] treats as lost data — so a throttle could be
/// recorded as data loss and clear a state row.
fn is_dynamodb_overloaded<E>(error: &AwsError<DynamoDbSdkError<E>>) -> bool
where
    E: ProvideErrorMetadata,
{
    let AwsError::AwsSdkError(sdk_error) = error else {
        return false;
    };

    match sdk_error {
        DynamoDbSdkError::TimeoutError(_) | DynamoDbSdkError::DispatchFailure(_) => true,
        DynamoDbSdkError::ServiceError(err) => {
            let status = err.raw().status().as_u16();

            status == 429
                || status >= 500
                || matches!(
                    err.err().code(),
                    Some(
                        "ThrottlingException"
                            | "ProvisionedThroughputExceededException"
                            | "RequestLimitExceeded"
                            | "InternalServerError"
                            | "ServiceUnavailable"
                    )
                )
        }
        _ => false,
    }
}

/// Mark a fragment as durably stored.
///
/// Durability is a fact about this store, not about the payload, so it is derived on read rather
/// than written down: an object present in the bucket is durable by definition. Persisting it would
/// mean serving one store's answer for another, and would let the claim outlive the object.
fn stored_durable(mut fragment: Fragment) -> Fragment {
    fragment.flags |= FragmentFlags::PayloadStoredDurable.bits();
    fragment
}

static STORE_ATTRIBUTES: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("store", "aws")]);

type BatchTaskResult = Result<(usize, StoreMatch), (usize, StoreError)>;

struct GetS3objectContentsOutput {
    read: usize,
    bytes: BytesMut,
    /// The fragment carried on the object, recovered from its object metadata. Arrives on the same
    /// response as the bytes it describes, so the two are necessarily from the same object version.
    fragment: Result<Fragment, ObjectMetadataError>,
}

pub struct AwsImmutableStore {
    s3: S3,
    dynamodb: DynamoDb,
    task_queue: TaskQueue<BatchTaskResult>,
    bucket: String,
    fragments_table_name: Arc<str>,
    /// Table of [`FragmentStateEntry`] rows. Named "metadata" for historical reasons; it holds
    /// lifecycle state only, never a fragment.
    fragment_state_table_name: Arc<str>,
    /// Set only where objects predating the move onto the S3 object may still exist. `None` is a
    /// deployment that has never written one, and reads accordingly refuse to guess.
    fragment_metadata_table_name: Option<Arc<str>>,
    force_write: bool,
    /// How long to wait between removing an association and counting what remains, so a put that
    /// had already passed its state probe has time to land its own association and be counted.
    obliteration_drain: Duration,
    latency_histogram: Histogram<f64>,
    labels_get: LabelArray,
    labels_put: LabelArray,
    labels_exist: LabelArray,
    labels_exist_batch: LabelArray,
    labels_obliterate: LabelArray,
    labels_query: LabelArray,
    labels_copy: LabelArray,
    labels_get_metadata: LabelArray,
    missing_payload_counter: Counter<u64>,
    labels_missing_payload: LabelArray,
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
        let labels_get_metadata = provider.get_labels_for_operation_context("get_metadata");
        let missing_payload_counter = provider.counter(METRICS_MISSING_PAYLOAD_METRIC_NAME);
        let labels_missing_payload = provider.get_labels_for_operation_context("missing_payload");
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
            fragment_state_table_name: Arc::from(
                settings.dynamodb.fragment_state_table_name.clone(),
            ),
            fragment_metadata_table_name: settings
                .dynamodb
                .fragment_metadata_table_name
                .as_ref()
                .map(|name| Arc::from(name.clone())),
            force_write: settings.force_write,
            obliteration_drain: Duration::from_millis(
                settings
                    .dynamodb
                    .timeout_millis
                    .max(MIN_OBLITERATION_DRAIN_MILLIS),
            ),
            latency_histogram,
            labels_get,
            labels_put,
            labels_exist,
            labels_exist_batch,
            labels_obliterate,
            labels_query,
            labels_copy,
            labels_get_metadata,
            missing_payload_counter,
            labels_missing_payload,
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

    /// Resolve an address to the fragment stored for it, without transferring the payload.
    ///
    /// An obliterated hash needs no special case: obliteration deletes the object, so the head
    /// returns not-found and the query reports a miss. Between an obliteration taking its mark and
    /// deleting the association there is a window where a partition that still holds a reference
    /// sees the fragment — which is accurate, since the payload survives for as long as any
    /// reference to it does.
    async fn do_query(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<(QueryResultSource, StoreQueryResult), StoreError> {
        let (match_made, state) = tokio::join!(
            self.lookup(repository, address, match_requested),
            self.load_state(address.hash)
        );

        let match_made = match_made?;
        let miss = Ok((
            QueryResultSource::State,
            StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone,
            },
        ));

        if match_made == StoreMatch::MatchNone {
            return miss;
        }

        match state? {
            Some(FragmentState::Stored) => Ok((
                QueryResultSource::State,
                StoreQueryResult {
                    fragment: stored_durable(Fragment::default()),
                    match_made,
                },
            )),
            Some(FragmentState::Obliterating | FragmentState::Obliterated) => {
                debug!("Query found obliterated fragment at address {address}");
                miss
            }
            None => {
                // if not in the `state` table then it could be a legacy fragment
                // that only exists in the metadata table
                if self.fragment_metadata_table_name.is_some()
                    && let Some(fragment) = self.fragment_from_metadata_table(address.hash).await?
                {
                    let legacy_fragment_state = FragmentState::from_bits(fragment.flags);

                    return match legacy_fragment_state {
                        FragmentState::Stored => {
                            let fragment = stored_durable(fragment);
                            Ok((
                                QueryResultSource::LegacyMetadata(fragment),
                                StoreQueryResult {
                                    fragment,
                                    match_made,
                                },
                            ))
                        }
                        FragmentState::Obliterating | FragmentState::Obliterated => {
                            debug!("Query found obliterated legacy fragment at address {address}");
                            miss
                        }
                    };
                }

                debug!("Query found an association at {address} with no stored payload");
                miss
            }
        }
    }

    /// Record that a payload exists, without disturbing an obliteration that may hold the hash.
    ///
    /// The create is conditional, so it can never overwrite a mark. Losing that condition is the
    /// ordinary outcome for content that is already stored — the row carries no representation, so
    /// there is nothing to reconcile and the existing row is already correct. The state it carries
    /// is returned so the caller can tell "already published" from "an obliteration holds this".
    async fn publish_state(&self, hash: Hash) -> Result<FragmentState, StoreError> {
        let entry = FragmentStateEntry::new(hash, FragmentState::Stored);
        let item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize fragment state for DynamoDB")
        })?;

        match self
            .dynamodb
            .put_item_conditional(&self.fragment_state_table_name, item, RowAbsent)
            .await
        {
            Ok(_) => Ok(FragmentState::Stored),
            Err(AwsError::AwsSdkError(DynamoDbSdkError::ServiceError(err)))
                if err.err().is_conditional_check_failed_exception() =>
            {
                let PutItemError::ConditionalCheckFailedException(failure) = err.err() else {
                    unreachable!()
                };

                Ok(failure
                    .item()
                    .and_then(|item| {
                        serde_dynamo::from_item::<_, FragmentStateEntry>(item.to_owned())
                            .inspect_err(|e| {
                                warn!("Failed to parse fragment state from item {item:?}: {e}");
                            })
                            .ok()
                    })
                    .map_or(FragmentState::Stored, |entry| entry.state()))
            }
            Err(e) => {
                warn!("Failed to publish fragment state for {hash}: {e:?}");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    Err(StoreError::from(SlowDown))
                } else {
                    Err(StoreError::internal_with_context(
                        e,
                        "DynamoDB fragment state write failed",
                    ))
                }
            }
        }
    }

    /// Record, and make repairable, a hash that is still referenced but whose payload is gone.
    ///
    /// Reaching here means the access check found an association while S3 reported no object, so
    /// the content was published once and has since been lost. The read is on its way back to the
    /// caller as an ordinary not-found, which is indistinguishable from content that was never
    /// stored, so this is the only point at which the difference is still known.
    ///
    /// Clearing the state row is what makes it repairable: with no row, the next put stops taking
    /// the "already durable" branch and uploads instead. That is safe only because the row holds no
    /// representation — there is nothing in it the next write does not re-derive.
    ///
    /// Both steps are best effort. Failing to clear the row leaves the hash exactly as it already
    /// was, and the alarm has been raised regardless.
    async fn report_missing_payload(&self, address: Address) {
        self.missing_payload_counter
            .add(1, &self.labels_missing_payload);
        error!(
            %address,
            "Fragment is referenced by a partition but absent from S3; content for this hash has \
             been lost. Clearing its state so the content can be stored again."
        );

        match self.load_state(address.hash).await {
            Ok(Some(FragmentState::Stored)) => {
                if let Err(error) = self.clear_state(address.hash).await {
                    warn!(%address, ?error, "Failed to clear state for a lost payload");
                }
            }
            Ok(state) => {
                debug!(%address, ?state, "Leaving state alone for a lost payload");
            }
            Err(error) => {
                warn!(%address, ?error, "Failed to read state for a lost payload");
            }
        }
    }

    /// Delete the state row for a hash, so the next put treats it as new content.
    ///
    /// Only called for a payload S3 has lost. An obliteration holding the mark is left alone by the
    /// caller, since removing a mark mid-obliteration would let a put republish underneath it.
    async fn clear_state(&self, hash: Hash) -> Result<(), StoreError> {
        let item = serde_dynamo::to_item(FragmentStateEntry::key(hash)).map_err(|e| {
            warn!("Failed to serialize fragment state key for {hash}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize fragment state for delete")
        })?;

        self.dynamodb
            .delete_item(&self.fragment_state_table_name, item)
            .await
            .map_err(|e| {
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment state delete failed")
                }
            })?;

        Ok(())
    }

    /// Move a tombstoned hash back to stored, now that its payload has been uploaded again.
    ///
    /// Losing this race is not a failure. Another writer reviving the same tombstone produced
    /// exactly the state this one wanted, and this one's bytes are already uploaded, so there is
    /// nothing left to disagree about. Only finding the hash back under an obliteration is a reason
    /// to stop, and that is a back-off rather than an error because the mark is transient.
    ///
    /// This tolerance belongs here rather than in [`AwsImmutableStore::advance_state`], which is
    /// also how an obliteration takes its mark — treating "already in the target state" as success
    /// there would let two obliterations both believe they hold it.
    async fn revive_state(&self, hash: Hash) -> Result<(), StoreError> {
        if self
            .advance_state(hash, FragmentState::Obliterated, FragmentState::Stored)
            .await
            .is_ok()
        {
            return Ok(());
        }

        match self.load_state(hash).await? {
            Some(FragmentState::Stored) => {
                debug!(%hash, "Another writer revived this hash first");
                Ok(())
            }
            state => {
                info!(%hash, ?state, "Hash is no longer revivable, asking the caller to retry");
                Err(StoreError::from(SlowDown))
            }
        }
    }

    /// Move the state row from one state to another, failing if it has moved underneath us.
    ///
    /// Obliteration uses this to take and release the mark. Because the row holds nothing but the
    /// state, this compare-and-set is over a single attribute and two writers racing for the mark
    /// cannot both win.
    async fn advance_state(
        &self,
        hash: Hash,
        expected: FragmentState,
        updated: FragmentState,
    ) -> Result<(), StoreError> {
        let entry = FragmentStateEntry::new(hash, updated);
        let item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize fragment state for DynamoDB")
        })?;

        match self
            .dynamodb
            .put_item_conditional(
                &self.fragment_state_table_name,
                item,
                StateUnchanged(expected),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(AwsError::AwsSdkError(DynamoDbSdkError::ServiceError(err)))
                if err.err().is_conditional_check_failed_exception() =>
            {
                warn!("Fragment state for {hash} was not {expected:?} when moving to {updated:?}");
                Err(StoreError::internal(
                    "Failed to update fragment state due to conflict",
                ))
            }
            Err(e) => {
                warn!("DynamoDB conditional put failed while updating state for {hash}: {e:?}");
                Err(StoreError::internal_with_context(
                    e,
                    "DynamoDB conditional fragment state update failed",
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

    pub(crate) async fn write_payload_and_state(
        &self,
        hash: Hash,
        fragment: Fragment,
        payload: Bytes,
    ) -> Result<(), StoreError> {
        if payload.len() != fragment.size_payload as usize {
            warn!(
                exepected_size = fragment.size_payload,
                received_size = payload.len(),
                %hash,
                "Failed to write fragment to immutable store for hash: payload size invalid"
            );
            return Err(StoreError::internal(format!(
                "Failed to store in immutable store for put {hash}"
            )));
        }

        {
            let mut dst = [0u8; 64];
            let s3_key = lore_revision::util::to_hex_str(hash.data(), &mut dst);

            self.s3
                .put_object(
                    self.bucket.as_str(),
                    s3_key,
                    payload.to_vec(),
                    Some(to_object_metadata(&fragment)),
                )
                .await
                .map(|_| ())
                .map_err(|error| {
                    warn!(?error, %hash, %s3_key, "Failed to write payload for hash");
                    if matches!(&error, AwsError::AwsSdkError(_)) {
                        StoreError::from(SlowDown)
                    } else {
                        StoreError::internal_with_context(error, "S3 put object failed")
                    }
                })?;
        }

        match self.publish_state(hash).await? {
            FragmentState::Stored => {}
            FragmentState::Obliterating => {
                info!(
                    %hash,
                    "Payload was uploaded while an obliteration holds the hash; \
                     leaving it unassociated and asking the caller to retry"
                );
                return Err(StoreError::from(SlowDown));
            }
            FragmentState::Obliterated => {
                info!(%hash, "Payload revives a tombstoned hash");
                self.revive_state(hash).await?;
            }
        }

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

    /// Read a fragment without its payload, from the object's object metadata.
    ///
    /// This is the one path that spends an S3 request purely on metadata, and it spends the
    /// cheapest one: `HeadObject` transfers no body. Reads that want the payload get the fragment
    /// for free on the `GetObject` response instead.
    async fn head_fragment(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let mut dst = [0u8; 64];
        let output = self
            .s3
            .head_object(
                self.bucket.as_str(),
                lore_revision::util::to_hex_str(hash.data(), &mut dst),
            )
            .await
            .map_err(|e| {
                if let AwsError::AwsSdkError(sdk_error) = e {
                    debug!(%hash, error = ?sdk_error, "head_fragment SDK error heading object");
                    match sdk_error.into_service_error() {
                        HeadObjectError::NotFound(_) => StoreError::from(AddressNotFound::from(
                            Address::zero_context_hash(hash),
                        )),
                        _ => StoreError::from(SlowDown),
                    }
                } else {
                    debug!(%hash, error = ?e, "head_fragment failed to head object");
                    StoreError::internal_with_context(e, "S3 head object failed")
                }
            })?;

        let fragment = match from_object_metadata(output.metadata()) {
            Ok(fragment) => fragment,
            Err(ObjectMetadataError::Absent) => {
                let legacy_metadata = self.fragment_from_metadata_table(hash).await?;
                legacy_metadata.ok_or_else(|| {
                    warn!(
                        %hash,
                        "Stored object carries no fragment metadata and no legacy row describes it"
                    );
                    StoreError::internal("S3 object carries no fragment metadata")
                })?
            }
            Err(e) => {
                warn!(%hash, "Stored object carries unusable fragment metadata: {e}");
                return Err(StoreError::internal_with_context(
                    e,
                    "S3 object fragment metadata unusable",
                ));
            }
        };

        Ok(stored_durable(fragment))
    }

    /// Read the lifecycle state of a hash. `None` means no row exists, so the hash is unknown.
    ///
    /// This is the cheap existence probe the whole design turns on: one strongly consistent
    /// `GetItem` answers "is this payload durable" for every partition at once, with no S3 request
    /// and no dependence on how many partitions reference it.
    pub(crate) async fn load_state(&self, hash: Hash) -> Result<Option<FragmentState>, StoreError> {
        let item = serde_dynamo::to_item(FragmentStateEntry::key(hash)).map_err(|e| {
            warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment entry for DynamoDB state load",
            )
        })?;

        let Some(av_map) = self
            .dynamodb
            .get_item(
                &self.fragment_state_table_name,
                item,
                true, /* consistent read */
            )
            .await
            .map_err(|e| {
                warn!(%hash, ?e, "Failed to get fragment state for hash");
                if is_dynamodb_overloaded(&e) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment state read failed")
                }
            })?
            .item
        else {
            return Ok(None);
        };

        let entry: FragmentStateEntry = serde_dynamo::from_item(av_map).map_err(|e| {
            warn!("Failed to deserialize fragment state: {e:?}");
            StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
        })?;

        Ok(Some(entry.state()))
    }

    /// Resolve the fragment for an object that carries none of its own.
    ///
    /// Reached only on [`ObjectMetadataError::Absent`] — an intact object with no lore metadata,
    /// which is exactly what an object written before the fragment moved onto it looks like. A
    /// `Malformed` object never arrives here: metadata that is present but unreadable means damage,
    /// and describing damaged bytes from a separate record is the mismatch this design exists to
    /// remove.
    ///
    /// With no legacy table configured there is nothing to fall back to, and nothing that should
    /// be: the deployment has declared it never wrote such an object.
    async fn fragment_from_metadata_table(
        &self,
        hash: Hash,
    ) -> Result<Option<Fragment>, StoreError> {
        let Some(table_name) = self.fragment_metadata_table_name.as_ref() else {
            warn!(
                %hash,
                "Stored object carries no fragment metadata and no fragment metadata table is \
                 configured; treating it as damaged"
            );
            return Err(StoreError::internal(
                "S3 object carries no fragment metadata",
            ));
        };

        let item = serde_dynamo::to_item(FragmentStateEntry::key(hash)).map_err(|e| {
            warn!("Failed to serialize legacy fragment key for {hash}: {e:?}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment entry for legacy metadata load",
            )
        })?;

        let entry = self
            .dynamodb
            .get_item(table_name, item, true /* consistent read */)
            .await
            .map_err(|e| {
                warn!(%hash, ?e, "Failed to read fragment metadata table");
                if is_dynamodb_overloaded(&e) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment metadata read failed")
                }
            })?
            .item
            .map(serde_dynamo::from_item::<_, FragmentMetadataEntry>)
            .transpose()
            .map_err(|e| {
                warn!(%hash, "Failed to deserialize fragment metadata row: {e:?}");
                StoreError::internal_with_context(e, "Fragment metadata row is unreadable")
            })?;

        Ok(entry.and_then(|entry| entry.fragment))
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

        let fragment = from_object_metadata(output.metadata());

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
            fragment,
        })
    }

    /// Check the object's own bytes against the fragment the same object declares.
    ///
    /// Both sides of this comparison come from one S3 response, so it is a self-consistency check
    /// on a single object rather than a comparison between two stores. It cannot fail because two
    /// records drifted apart; only because the object itself is damaged.
    fn read_payload(
        s3_contents: GetS3objectContentsOutput,
        hash: Hash,
        fragment: Fragment,
    ) -> Result<Bytes, StoreError> {
        let payload_size = fragment.size_payload as usize;
        let buffer_size = s3_contents.bytes.len();

        if buffer_size == payload_size {
            Ok(s3_contents.bytes.freeze())
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

    /// Load a payload and the fragment describing it, in a single S3 request.
    ///
    /// There is no `DynamoDB` read here at all. The fragment arrives as object metadata on the very
    /// response carrying the bytes, so it describes those bytes by construction — no second record
    /// to consult, and nothing that can be stale with respect to what was read.
    pub(crate) async fn load(&self, hash: Hash) -> Result<(Fragment, Bytes), StoreError> {
        let s3_contents = self.get_s3_object_contents(hash).await?;

        let fragment = match s3_contents.fragment {
            Ok(fragment) => fragment,
            Err(ObjectMetadataError::Absent) => {
                let legacy_metadata = self.fragment_from_metadata_table(hash).await?;
                legacy_metadata.ok_or_else(|| {
                    warn!(
                        %hash,
                        "Stored object carries no fragment metadata and no legacy row describes it"
                    );
                    StoreError::internal("S3 object carries no fragment metadata")
                })?
            }
            Err(e) => {
                warn!(%hash, "Stored object carries unusable fragment metadata: {e}");
                return Err(StoreError::internal_with_context(
                    e,
                    "S3 object fragment metadata unusable",
                ));
            }
        };

        let fragment = stored_durable(fragment);
        lore_storage::validate_fragment_size(&fragment)?;

        let payload = Self::read_payload(s3_contents, hash, fragment)?;
        Ok((fragment, payload))
    }

    /// Obliterate the fragments a fragmented payload points at, if it is one.
    ///
    /// Called once the mark is held and no association remains, so the parent payload is still
    /// present to be read and nothing can be adding references beneath it.
    async fn obliterate_sub_fragments(
        self: Arc<Self>,
        repository: Context,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let (fragment, payload) = match self.load(address.hash).await {
            Ok(loaded) => loaded,
            Err(e) if e.is_address_not_found() => {
                info!("Payload for {address} is already gone, no sub-fragments to obliterate");
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        if fragment.flags & FragmentFlags::PayloadFragmented == 0 {
            return Ok(());
        }

        let payload = payload.to_aligned::<FragmentReference>();
        let sub_fragments = payload.as_type_slice::<FragmentReference>();
        info!(
            "Fragment {address} has {} sub-fragments",
            sub_fragments.len()
        );

        let span = tracing::Span::current();
        let mut join_set = JoinSet::new();
        for reference in sub_fragments.iter() {
            let self_clone = self.clone();
            let stats = stats.clone();
            let sub_address = Address {
                hash: reference.hash,
                context: address.context,
            };

            info!("Spawning task to obliterate {sub_address}");
            lore_base::lore_spawn!(
                join_set,
                async move {
                    self_clone
                        .obliterate(repository.into(), sub_address, stats)
                        .await
                        .map_err(|e| (sub_address, e))
                }
                .instrument(span.clone())
            );
        }

        let mut failures = false;
        while let Some(result) = join_set.join_next().await {
            match result {
                Err(e) => {
                    failures = true;
                    warn!("Failed to join task for fragment reference obliterate: {e:?}");
                }
                Ok(Err((sub_address, e))) => {
                    failures = true;
                    warn!("Obliteration failed for sub-fragment {sub_address}: {e:?}");
                }
                Ok(Ok(())) => {}
            }
        }

        if failures {
            warn!("Obliteration failed for at least one sub-fragment.");
            return Err(StoreError::internal(format!(
                "Failed to obliterate immutable {address}"
            )));
        }

        info!("Done obliterating sub-fragments");
        Ok(())
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
            let (_, query_result) =
                Box::pin(self.do_query(repository, address, match_requested)).await?;
            Ok(query_result)
        })
        .into()
    }

    /// Unlike [`AwsImmutableStore::query`], this reads the object to report the representation
    /// actually stored, which costs a `HeadObject`. It transfers no body, and it is the only path
    /// in this store that spends an S3 request purely on metadata.
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "AwsImmutableStore::get_metadata" skip(self))]
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreQueryResult, StoreError> {
        let repository: Context = partition.into();
        timed!(self.latency_histogram, &self.labels_get_metadata, {
            let miss = StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone,
            };

            let (query_source, query_result) =
                Box::pin(self.do_query(repository, address, StoreMatch::MatchFull)).await?;
            let match_made = query_result.match_made;

            if match_made == StoreMatch::MatchNone {
                return Ok(miss);
            }

            match query_source {
                QueryResultSource::LegacyMetadata(fragment) => Ok(StoreQueryResult {
                    fragment,
                    match_made,
                }),
                QueryResultSource::State => match self.head_fragment(address.hash).await {
                    Ok(fragment) => Ok(StoreQueryResult {
                        fragment,
                        match_made,
                    }),
                    Err(e) if e.is_address_not_found() => {
                        self.report_missing_payload(address).await;
                        Ok(miss)
                    }
                    Err(e) => Err(e),
                },
            }
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
                    Some(r) => r,
                    None => load_fut.await,
                };

                if load_output
                    .as_ref()
                    .err()
                    .is_some_and(StoreError::is_address_not_found)
                {
                    self.report_missing_payload(address).await;
                }

                load_output
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
        timed!(self.latency_histogram, &self.labels_put, {
            let probe = if self.force_write {
                (None, false)
            } else {
                let entry = FragmentsEntry::new(repository, address);
                let (associated, state) =
                    tokio::join!(self.exists_exact(&entry), self.load_state(address.hash));
                (state?, associated?)
            };

            match probe {
                (Some(FragmentState::Obliterating), _) => {
                    info!(
                        "Received request to put fragment at {address} that is in the process of \
                         being obliterated"
                    );
                    Err(StoreError::from(SlowDown))
                }

                (Some(FragmentState::Stored), true) => Ok(()),

                (Some(FragmentState::Stored), false) if payload.is_some() => {
                    self.associate_fragment(repository, address).await
                }

                (Some(FragmentState::Stored), false) => {
                    Err(StoreError::internal("Payload buffer required"))
                }

                _ => match payload {
                    Some(payload) => {
                        self.write_payload_and_state(address.hash, fragment, payload)
                            .await?;
                        self.associate_fragment(repository, address).await?;
                        Ok(())
                    }
                    None => Err(StoreError::internal("Payload buffer required")),
                },
            }
        })
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

            let Some(state) = self
                .load_state(address.hash)
                .instrument(span.clone())
                .await?
            else {
                info!("No fragment state for {address}, nothing to obliterate");
                return Ok(());
            };

            if state.is_obliteration() {
                info!("Fragment {address} is already being, or has already been, obliterated");
                return Ok(());
            }

            self.advance_state(
                address.hash,
                FragmentState::Stored,
                FragmentState::Obliterating,
            )
            .instrument(span.clone())
            .await?;
            info!("Acquired obliteration mark for {address}");

            self.delete_association(repository, address)
                .instrument(span.clone())
                .await?;
            stats
                .num_fragments
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            tokio::time::sleep(self.obliteration_drain).await;

            info!("Association deleted, re-checking for other associations...");
            if self
                .has_associations(address.hash)
                .instrument(span.clone())
                .await?
            {
                info!("Fragment still associated, releasing the obliteration mark");
                return self
                    .advance_state(
                        address.hash,
                        FragmentState::Obliterating,
                        FragmentState::Stored,
                    )
                    .instrument(span.clone())
                    .await
                    .inspect_err(|e| {
                        warn!("Failed to release the obliteration mark: {e:?}");
                    });
            }

            self.clone()
                .obliterate_sub_fragments(repository, address, stats.clone())
                .instrument(span.clone())
                .await?;

            self.delete_payload(address.hash)
                .instrument(span.clone())
                .await?;

            stats
                .num_payloads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            self.advance_state(
                address.hash,
                FragmentState::Obliterating,
                FragmentState::Obliterated,
            )
            .await
            .inspect_err(|e| {
                warn!("Failed to finalize obliterate for {address}: {e:?}");
            })
        })
        .into()
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
            let match_made = self
                .lookup(source_repository, source_address, StoreMatch::MatchFull)
                .await?;

            if match_made != StoreMatch::MatchFull {
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
    use std::sync::atomic::Ordering;

    use lore_base::types::FragmentFlags;
    use lore_storage::ImmutableStore;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::store::object_metadata::PAYLOAD_FLAGS;
    use crate::store::test_util::*;

    #[tokio::test]
    async fn put_stores_the_fragment_on_the_object() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        store(&fake)
            .await
            .put(
                repository.into(),
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put should succeed");

        assert_eq!(fake.stored_fragment(address.hash), Some(fragment));
        assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        assert_eq!(fake.association_count(address.hash), 1);
    }

    #[tokio::test]
    async fn put_of_an_already_associated_fragment_writes_nothing() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                repository.into(),
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("first put should succeed");

        let (other, other_payload) = representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
        store
            .put(
                repository.into(),
                address,
                other,
                Some(other_payload),
                false,
            )
            .await
            .expect("second put should succeed");

        assert_eq!(
            fake.stored_fragment(address.hash),
            Some(fragment),
            "an already associated put must not re-upload"
        );
        assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
    }

    #[tokio::test]
    async fn put_deduplicates_across_partitions_without_uploading() {
        let fake = Fake::default();
        let hash: Hash = random();
        let first = Address {
            hash,
            context: random(),
        };
        let second = Address {
            hash,
            context: random(),
        };
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                random::<Context>().into(),
                first,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("first put should succeed");

        let (other, other_payload) = representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
        store
            .put(
                random::<Context>().into(),
                second,
                other,
                Some(other_payload),
                false,
            )
            .await
            .expect("cross-partition put should succeed");

        assert_eq!(
            fake.stored_fragment(hash),
            Some(fragment),
            "deduplication must leave the stored representation alone"
        );
        assert_eq!(fake.association_count(hash), 2);
    }

    #[tokio::test]
    async fn put_without_a_payload_may_not_claim_stored_content() {
        let fake = Fake::default();
        let hash: Hash = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                random::<Context>().into(),
                Address {
                    hash,
                    context: random(),
                },
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect("first put should succeed");

        let claimed = Address {
            hash,
            context: random(),
        };
        store
            .put(random::<Context>().into(), claimed, fragment, None, false)
            .await
            .expect_err("a hash alone is not evidence the caller holds the content");

        assert_eq!(fake.association_count(hash), 1);
    }

    #[tokio::test]
    async fn get_reads_the_fragment_from_the_object_it_returns() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                repository.into(),
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put should succeed");

        let (loaded, bytes) = store
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("get should succeed");

        assert_eq!(bytes, payload);
        assert_eq!(loaded.size_payload, fragment.size_payload);
        assert_eq!(loaded.size_content, fragment.size_content);
        assert_eq!(
            loaded.flags & FragmentFlags::PayloadStoredDurable,
            FragmentFlags::PayloadStoredDurable,
            "durability is derived from the object existing, not read from a record"
        );
    }

    /// Query answers from `DynamoDB` alone. It is on the ingress write path, once per fragment
    /// stored, so it must not reach S3 — which means it reports whether the payload is there and
    /// durable, not what representation is stored.
    #[tokio::test]
    async fn query_reports_a_match_without_reading_s3() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let result = store
            .query(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("query should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert_eq!(
            result.fragment.flags & FragmentFlags::PayloadStoredDurable,
            FragmentFlags::PayloadStoredDurable
        );
        assert_eq!(
            fake.object_reads(),
            0,
            "query must not touch S3; it is called once per fragment on the ingress write path"
        );
    }

    /// The representation a put stored must come back from a later metadata read — the whole
    /// reason this path exists separately from `query`, which cannot report it.
    #[tokio::test]
    async fn get_metadata_returns_the_representation_that_was_stored() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let result = store
            .get_metadata(repository.into(), address)
            .await
            .expect("get_metadata should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert_eq!(result.fragment.size_payload, fragment.size_payload);
        assert_eq!(result.fragment.size_content, fragment.size_content);
        assert_eq!(
            result.fragment.flags & PAYLOAD_FLAGS,
            fragment.flags & PAYLOAD_FLAGS,
            "the stored compression must survive the round trip"
        );
        assert_eq!(
            result.fragment.flags & FragmentFlags::PayloadStoredDurable,
            FragmentFlags::PayloadStoredDurable
        );
        assert_eq!(
            fake.object_reads(),
            1,
            "exactly one HeadObject, and only on this path"
        );
    }

    #[tokio::test]
    async fn get_metadata_reads_a_preexisting_object_from_the_fragment_metadata_table() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, _) = preexisting_object(&fake, repository, address);

        let result = migrated_store(&fake)
            .await
            .get_metadata(repository.into(), address)
            .await
            .expect("get_metadata should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert_eq!(result.fragment.size_payload, fragment.size_payload);
        assert_eq!(result.fragment.size_content, fragment.size_content);
    }

    #[tokio::test]
    async fn get_metadata_reports_a_miss_for_an_unknown_address() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };

        let result = store(&fake)
            .await
            .get_metadata(random::<Context>().into(), address)
            .await
            .expect("get_metadata should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchNone);
        assert_eq!(fake.object_reads(), 0, "a miss must not reach S3");
    }

    #[tokio::test]
    async fn query_reports_a_miss_when_no_state_row_exists() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();

        fake.add_association(repository, address);

        let result = store(&fake)
            .await
            .query(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("query should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchNone);
    }

    #[tokio::test]
    async fn force_write_replaces_the_stored_representation() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        store(&fake)
            .await
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let (replacement, replacement_payload) =
            representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
        store_with(&fake, true, false)
            .await
            .put(
                repository.into(),
                address,
                replacement,
                Some(replacement_payload.clone()),
                false,
            )
            .await
            .expect("forced put should succeed");

        assert_eq!(fake.stored_fragment(address.hash), Some(replacement));
        assert_eq!(
            fake.object(address.hash).unwrap().0,
            replacement_payload.as_ref()
        );
    }

    #[tokio::test]
    async fn put_backs_off_while_an_obliteration_holds_the_hash() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.set_state(address.hash, FragmentState::Obliterating);

        let error = store(&fake)
            .await
            .put(
                random::<Context>().into(),
                address,
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect_err("a put racing an obliteration must back off");

        assert!(error.is_slow_down(), "expected a retryable back-off");
        assert_eq!(fake.association_count(address.hash), 0);
    }

    /// Sets up an object as it was stored before the fragment moved onto it: bare bytes in S3, the
    /// fragment in a table row.
    fn preexisting_object(fake: &Fake, repository: Context, address: Address) -> (Fragment, Bytes) {
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.put_object_without_metadata(address.hash, payload.as_ref());
        fake.set_fragment_metadata_row(address.hash, fragment);
        fake.add_association(repository, address);

        (fragment, payload)
    }

    #[tokio::test]
    async fn get_falls_back_to_the_legacy_row_for_an_object_with_no_metadata() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = preexisting_object(&fake, repository, address);

        let (loaded, bytes) = migrated_store(&fake)
            .await
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("an object written before the cut-over must still be readable");

        assert_eq!(bytes, payload);
        assert_eq!(loaded.size_payload, fragment.size_payload);
        assert_eq!(loaded.size_content, fragment.size_content);
        assert_eq!(
            loaded.flags & FragmentFlags::PayloadStoredDurable,
            FragmentFlags::PayloadStoredDurable
        );
    }

    /// A pre-cut-over object is answered from its state row like any other, with no fallback read:
    /// query never needs the representation, so it never needs the fragment metadata table either.
    #[tokio::test]
    async fn query_matches_a_preexisting_object_without_reading_s3() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        preexisting_object(&fake, repository, address);

        let result = migrated_store(&fake)
            .await
            .query(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("query should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert_eq!(fake.object_reads(), 0, "query must not touch S3");
    }

    /// A deployment that never wrote an object without metadata should not go looking for a row
    /// describing one. An object in that shape is damaged, not old.
    #[tokio::test]
    async fn get_refuses_an_object_with_no_metadata_when_no_legacy_table_is_configured() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        preexisting_object(&fake, repository, address);

        store(&fake)
            .await
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("without a legacy table configured there is nothing to fall back to");
    }

    /// Metadata that is present but unreadable means a damaged object. Describing it from a
    /// separate record is exactly the mismatch this design removes, so it must not fall back even
    /// where a legacy row exists.
    #[tokio::test]
    async fn get_does_not_fall_back_for_an_object_with_damaged_metadata() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.put_object_with_damaged_metadata(address.hash, payload.as_ref());
        fake.set_fragment_metadata_row(address.hash, fragment);
        fake.add_association(repository, address);

        migrated_store(&fake)
            .await
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("a damaged object must not be described by a legacy row");
    }

    mod separate_metadata_table {
        use super::*;

        /// When the metadata table IS configured but holds no row for a hash that has an
        /// association but no state row, `query` must still return a miss. The metadata-table
        /// check in `do_query` must not turn a genuine miss into a phantom match.
        #[tokio::test]
        async fn query_misses_when_no_state_or_legacy_row() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let repository: Context = random();
            fake.add_association(repository, address);
            // No state row, no legacy metadata row.

            let result = store_with_separate_metadata_table(&fake)
                .await
                .query(repository.into(), address, StoreMatch::MatchFull)
                .await
                .expect("query should succeed even when no row is found");

            assert_eq!(result.match_made, StoreMatch::MatchNone);
        }

        /// A legacy fragment whose flags carry obliteration bits must not be returned as a match.
        /// The state table has no row (pre-state-table era), but the metadata row records that the
        /// fragment was obliterated — `do_query` must treat it the same as a state-row obliteration.
        #[tokio::test]
        async fn query_misses_for_an_obliterated_legacy_fragment() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let repository: Context = random();
            let obliterated = Fragment {
                flags: FragmentFlags::PayloadObliterated.bits(),
                size_payload: 64,
                size_content: 256,
            };

            fake.add_association(repository, address);
            fake.set_legacy_metadata_row(address.hash, obliterated);
            // No state row — the obliteration bit lives only in the legacy metadata flags.

            let result = store_with_separate_metadata_table(&fake)
                .await
                .query(repository.into(), address, StoreMatch::MatchFull)
                .await
                .expect("query should succeed");

            assert_eq!(result.match_made, StoreMatch::MatchNone);
        }

        /// A fragment stored before the state table existed: an association exists, no state row,
        /// but the metadata table holds the legacy fragment description. `query` must report a
        /// match — the new `None` branch in `do_query` exists precisely for this scenario.
        #[tokio::test]
        async fn query_matches_a_legacy_fragment_with_no_state_row() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let repository: Context = random();
            let (fragment, _) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            fake.add_association(repository, address);
            fake.set_legacy_metadata_row(address.hash, fragment);
            // No state row — `load_state` returns `Ok(None)`.

            let result = store_with_separate_metadata_table(&fake)
                .await
                .query(repository.into(), address, StoreMatch::MatchFull)
                .await
                .expect("a legacy fragment with no state row must be queryable");

            assert_eq!(result.match_made, StoreMatch::MatchFull);
        }

        /// When `do_query` returns `QueryResultSource::LegacyMetadata`, `get_metadata` must use
        /// the fragment it already obtained from the metadata table rather than reading S3. An
        /// object read here would be redundant and penalise every `get_metadata` call for
        /// pre-cutover data.
        ///
        /// The returned fragment must carry `PayloadStoredDurable` (set by `do_query` when it
        /// takes the `LegacyMetadata` branch) and must preserve the original flags from the
        /// metadata row.
        #[tokio::test]
        async fn get_metadata_uses_legacy_metadata_without_reading_s3() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let repository: Context = random();
            let (mut fragment, payload) =
                representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
            fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

            fake.add_association(repository, address);
            fake.set_legacy_metadata_row(address.hash, fragment);
            fake.put_object_without_metadata(address.hash, payload.as_ref());
            // No state row — `do_query` returns `QueryResultSource::LegacyMetadata`.

            let result = store_with_separate_metadata_table(&fake)
                .await
                .get_metadata(repository.into(), address)
                .await
                .expect("get_metadata must succeed for a legacy fragment");

            assert_eq!(result.match_made, StoreMatch::MatchFull);
            assert_eq!(result.fragment.size_payload, fragment.size_payload);
            assert_eq!(result.fragment.size_content, fragment.size_content);
            assert_eq!(
                result.fragment.flags & FragmentFlags::PayloadStoredDurable,
                FragmentFlags::PayloadStoredDurable,
                "do_query must mark legacy metadata fragments as durably stored"
            );
            assert_eq!(
                result.fragment.flags & FragmentFlags::PayloadDoNotReplicate,
                FragmentFlags::PayloadDoNotReplicate,
                "original flags from the metadata row must be preserved"
            );
            assert_eq!(
                fake.object_reads(),
                0,
                "S3 must not be read when the fragment came from the metadata table"
            );
        }

        /// When `head_fragment` falls back to `fragment_from_metadata_table` and finds no row
        /// (`Ok(None)`), `get_metadata` must return an error. This covers the caller-side
        /// `ok_or_else` added in `head_fragment`.
        #[tokio::test]
        async fn get_metadata_fails_when_object_has_no_s3_metadata_and_no_legacy_row() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let repository: Context = random();
            let (_, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            fake.add_association(repository, address);
            fake.set_state(address.hash, FragmentState::Stored);
            fake.put_object_without_metadata(address.hash, payload.as_ref());
            // No metadata row — `fragment_from_metadata_table` returns `Ok(None)`.

            store_with_separate_metadata_table(&fake)
                .await
                .get_metadata(repository.into(), address)
                .await
                .expect_err("an object with no S3 metadata and no legacy row must not be returned");
        }

        /// The same `ok_or_else` guard on the `get_s3_object_contents` path: when `get` reads an
        /// object with no S3 metadata and the metadata table holds no row either, it must fail.
        #[tokio::test]
        async fn get_fails_when_object_has_no_s3_metadata_and_no_legacy_row() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let repository: Context = random();
            let (_, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            fake.add_association(repository, address);
            fake.set_state(address.hash, FragmentState::Stored);
            fake.put_object_without_metadata(address.hash, payload.as_ref());
            // No legacy row — `fragment_from_metadata_table` returns `Ok(None)`, which
            // `get_s3_object_contents` maps to an error.

            store_with_separate_metadata_table(&fake)
                .await
                .get(repository.into(), address, StoreMatch::MatchFull)
                .await
                .expect_err("an object with no S3 metadata and no legacy row must not be returned");
        }
    }

    /// A lost payload must not stay lost. Clearing the state row is what lets the next put stop
    /// short-circuiting on "already durable" and upload the content again.
    #[tokio::test]
    async fn a_read_of_a_lost_payload_clears_its_state_so_a_put_can_restore_it() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                repository.into(),
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);

        let error = store
            .clone()
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("a lost payload reads as not found");
        assert!(error.is_address_not_found());

        assert_eq!(
            fake.state_of(address.hash),
            None,
            "the state row must be cleared so the hash stops looking durable"
        );

        store
            .clone()
            .put(
                repository.into(),
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("re-put should succeed");

        assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));

        let (_, restored) = store
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("the payload should be readable again");
        assert_eq!(restored, payload);
    }

    /// A lost payload must be reported however it is found. `get_metadata` is the cheaper call, so
    /// a client that only ever reads metadata would otherwise never raise the alarm.
    #[tokio::test]
    async fn get_metadata_of_a_lost_payload_reports_and_repairs_it() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);

        let result = store
            .get_metadata(repository.into(), address)
            .await
            .expect("a lost payload reports a miss");

        assert_eq!(result.match_made, StoreMatch::MatchNone);
        assert_eq!(
            fake.state_of(address.hash),
            None,
            "get_metadata must repair the loss it found, exactly as get does"
        );
    }

    /// An obliteration in flight owns the hash. Its mark must survive a read that races the payload
    /// deletion, or a put could republish underneath it.
    #[tokio::test]
    async fn a_read_of_a_lost_payload_leaves_an_obliteration_mark_alone() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);
        fake.set_state(address.hash, FragmentState::Obliterating);

        store
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("a lost payload reads as not found");

        assert_eq!(
            fake.state_of(address.hash),
            Some(FragmentState::Obliterating),
            "a read must not clear a mark an obliteration is holding"
        );
    }

    #[tokio::test]
    async fn put_revives_a_tombstoned_hash() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.set_state(address.hash, FragmentState::Obliterated);

        store(&fake)
            .await
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("re-upload over a tombstone is allowed");

        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        assert_eq!(fake.stored_fragment(address.hash), Some(fragment));
        assert_eq!(fake.association_count(address.hash), 1);
    }

    /// Store a fragmented parent whose payload is a list of references to `leaves`, all under one
    /// repository and context so obliteration walks from the parent into each leaf.
    async fn store_fragmented(
        store: &Arc<AwsImmutableStore>,
        repository: Context,
        context: Context,
        leaves: &[Hash],
    ) -> Address {
        const LEAF_CONTENT: u64 = 256;

        for (index, hash) in leaves.iter().enumerate() {
            let (fragment, payload) = representation(
                FragmentFlags::PayloadCompressedZstd,
                64 + index,
                LEAF_CONTENT,
            );
            store
                .clone()
                .put(
                    repository.into(),
                    Address {
                        hash: *hash,
                        context,
                    },
                    fragment,
                    Some(payload),
                    false,
                )
                .await
                .expect("leaf put should succeed");
        }

        let references: Vec<FragmentReference> = leaves
            .iter()
            .enumerate()
            .map(|(index, hash)| FragmentReference {
                hash: *hash,
                offset_content: index as u64 * LEAF_CONTENT,
            })
            .collect();

        let payload = Bytes::from(references.as_bytes().to_vec());
        let parent = Address {
            hash: random(),
            context,
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: u32::try_from(payload.len()).unwrap(),
            size_content: LEAF_CONTENT * leaves.len() as u64,
        };

        store
            .clone()
            .put(repository.into(), parent, fragment, Some(payload), false)
            .await
            .expect("parent put should succeed");

        parent
    }

    #[tokio::test]
    async fn obliterate_recurses_into_sub_fragments() {
        let fake = Fake::default();
        let repository: Context = random();
        let context: Context = random();
        let leaves = [random::<Hash>(), random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, repository, context, &leaves).await;

        let stats = Arc::new(StoreObliterateStats::default());
        store
            .obliterate(repository.into(), parent, stats.clone())
            .await
            .expect("obliterate should succeed");

        assert!(fake.object(parent.hash).is_none(), "parent payload remains");
        assert_eq!(fake.association_count(parent.hash), 0);

        for leaf in leaves {
            assert!(
                fake.object(leaf).is_none(),
                "a sub-fragment payload was not obliterated"
            );
            assert_eq!(fake.association_count(leaf), 0);
            assert_eq!(fake.state_of(leaf), Some(FragmentState::Obliterated));
        }

        assert_eq!(
            stats.num_fragments.load(Ordering::Relaxed),
            3,
            "the parent and both sub-fragments should each be counted"
        );
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 3);
    }

    /// Recursion must respect each sub-fragment's own reference count. A leaf another partition
    /// still holds survives, and its mark is released so the hash stays writable.
    #[tokio::test]
    async fn obliterate_keeps_a_sub_fragment_another_partition_references() {
        let fake = Fake::default();
        let repository: Context = random();
        let context: Context = random();
        let shared = random::<Hash>();
        let leaves = [shared, random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, repository, context, &leaves).await;

        let other = Address {
            hash: shared,
            context: random(),
        };
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
        store
            .clone()
            .put(
                random::<Context>().into(),
                other,
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect("second partition put should succeed");

        store
            .obliterate(
                repository.into(),
                parent,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate should succeed");

        assert!(
            fake.object(shared).is_some(),
            "a sub-fragment referenced elsewhere must survive"
        );
        assert_eq!(fake.association_count(shared), 1);
        assert_eq!(
            fake.state_of(shared),
            Some(FragmentState::Stored),
            "the surviving sub-fragment must not be left marked"
        );

        assert!(fake.object(leaves[1]).is_none(), "unshared leaf remains");
        assert!(fake.object(parent.hash).is_none(), "parent remains");
    }

    /// The parent's payload must still be readable when recursion runs, since that is where the
    /// reference list comes from — it is deleted only after the sub-fragments are handled.
    #[tokio::test]
    async fn obliterate_reads_the_reference_list_before_deleting_the_parent() {
        let fake = Fake::default();
        let repository: Context = random();
        let context: Context = random();
        let leaves = [random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, repository, context, &leaves).await;

        store
            .obliterate(
                repository.into(),
                parent,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate should succeed");

        assert!(
            fake.object(leaves[0]).is_none(),
            "the reference list was not read, so the sub-fragment survived"
        );
    }

    #[tokio::test]
    async fn obliterate_removes_the_reference_and_the_payload() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let stats = Arc::new(StoreObliterateStats::default());
        store
            .obliterate(repository.into(), address, stats.clone())
            .await
            .expect("obliterate should succeed");

        assert_eq!(fake.association_count(address.hash), 0);
        assert!(fake.object(address.hash).is_none());
        assert_eq!(
            fake.state_of(address.hash),
            Some(FragmentState::Obliterated)
        );
        assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn obliterate_keeps_the_payload_while_another_partition_references_it() {
        let fake = Fake::default();
        let hash: Hash = random();
        let mine = Address {
            hash,
            context: random(),
        };
        let theirs = Address {
            hash,
            context: random(),
        };
        let repository: Context = random();
        let other_repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                repository.into(),
                mine,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put should succeed");
        store
            .clone()
            .put(
                other_repository.into(),
                theirs,
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect("second put should succeed");

        store
            .obliterate(
                repository.into(),
                mine,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate should succeed");

        assert_eq!(fake.association_count(hash), 1);
        assert!(
            fake.object(hash).is_some(),
            "compliance only requires the obliterated partition's reference to be gone"
        );
        assert_eq!(
            fake.state_of(hash),
            Some(FragmentState::Stored),
            "the mark must be released so the hash stays writable"
        );
    }

    // ---------------------------------------------------------------------
    // Failure paths
    // ---------------------------------------------------------------------

    /// A timeout means the answer is unknown, so the caller must be told to retry. Any other
    /// `DynamoDB` failure means the hash could not be resolved, which reads as not found.
    #[tokio::test]
    async fn a_state_read_timeout_asks_the_caller_to_retry() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.fail(Fault::StateReadTimeout);

        let error = store(&fake)
            .await
            .put(
                random::<Context>().into(),
                address,
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect_err("a timeout must not be reported as a definite answer");

        assert!(error.is_slow_down(), "expected a retryable back-off");
    }

    #[tokio::test]
    async fn a_throttled_state_read_asks_the_caller_to_retry() {
        let fake = Fake::default();
        fake.fail(Fault::StateRead);

        let error = store(&fake)
            .await
            .load_state(random())
            .await
            .expect_err("throttling is not an answer");

        assert!(error.is_slow_down(), "throttling must be retryable");
    }

    /// A failed read must never read as a miss. A caller told "not found" for a hash it references
    /// treats the content as lost, which counts data loss and clears the state row — off a
    /// `DynamoDB` error that says nothing about whether the content is there.
    #[tokio::test]
    async fn a_broken_state_read_is_an_error_not_a_miss() {
        let fake = Fake::default();
        fake.fail(Fault::StateReadBroken);

        let error = store(&fake)
            .await
            .load_state(random())
            .await
            .expect_err("a broken read is not an empty table");

        assert!(!error.is_address_not_found(), "must not read as a miss");
        assert!(!error.is_slow_down(), "and is not retryable either");
    }

    /// The same rule on the fallback path, where the consequence is sharpest: this read only
    /// happens for a hash a partition references, so a miss here is what triggers the repair.
    #[tokio::test]
    async fn a_failed_fragment_metadata_read_does_not_clear_the_state_row() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        preexisting_object(&fake, repository, address);

        let store = migrated_store(&fake).await;
        fake.fail(Fault::StateRead);

        let error = store
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("the metadata read is throttled");

        assert!(error.is_slow_down());
        assert!(
            fake.state_of(address.hash).is_some(),
            "a throttle must not be mistaken for a lost payload and clear the row"
        );
    }

    /// Two writers reviving the same tombstone both want the hash stored. The one that loses the
    /// compare-and-set has already uploaded its bytes and got the state it wanted, so it must not
    /// fail.
    #[tokio::test]
    async fn reviving_a_hash_another_writer_already_revived_succeeds() {
        let fake = Fake::default();
        let hash: Hash = random();

        fake.set_state(hash, FragmentState::Stored);

        store(&fake)
            .await
            .revive_state(hash)
            .await
            .expect("losing the revival race is not a failure");
    }

    #[tokio::test]
    async fn reviving_a_hash_an_obliteration_retook_backs_off() {
        let fake = Fake::default();
        let hash: Hash = random();

        fake.set_state(hash, FragmentState::Obliterating);

        let error = store(&fake)
            .await
            .revive_state(hash)
            .await
            .expect_err("an obliteration holds the hash again");

        assert!(error.is_slow_down(), "the mark is transient, so back off");
    }

    /// The upload and the state row survive an association failure, so a retry finishes the job
    /// without re-uploading. Nothing is left that a later put would mistake for a complete write.
    #[tokio::test]
    async fn a_put_that_cannot_associate_leaves_the_payload_ready_for_a_retry() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.fail(Fault::AssociationWrite);

        store(&fake)
            .await
            .put(
                repository.into(),
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect_err("the association write fails");

        assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        assert_eq!(fake.association_count(address.hash), 0);

        let recovered = Fake::default();
        recovered.set_state(address.hash, FragmentState::Stored);
        recovered.put_object_without_metadata(address.hash, payload.as_ref());
        store(&recovered)
            .await
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("a retry associates without re-uploading");
        assert_eq!(recovered.association_count(address.hash), 1);
    }

    /// Compliance is discharged by the association delete. If the count that follows fails, the
    /// obliteration reports failure and holds its mark — but the reference is already gone.
    #[tokio::test]
    async fn an_obliterate_that_cannot_count_references_still_removed_the_reference() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.fail(Fault::AssociationCount);

        store
            .obliterate(
                repository.into(),
                address,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect_err("counting references fails");

        assert_eq!(
            fake.association_count(address.hash),
            0,
            "the compliance obligation is discharged before anything that can fail after it"
        );
        assert_eq!(
            fake.state_of(address.hash),
            Some(FragmentState::Obliterating),
            "the mark is held, which is the known crashed-obliteration gap"
        );
    }

    #[tokio::test]
    async fn an_obliterate_that_cannot_delete_the_payload_does_not_tombstone_it() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.fail(Fault::ObjectDelete);

        store
            .obliterate(
                repository.into(),
                address,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect_err("deleting the payload fails");

        assert!(
            fake.object(address.hash).is_some(),
            "the payload survives a failed delete"
        );
        assert_eq!(
            fake.state_of(address.hash),
            Some(FragmentState::Obliterating),
            "it must not be tombstoned while its payload is still there"
        );
    }

    #[tokio::test]
    async fn an_obliterate_that_cannot_list_versions_does_not_tombstone_the_payload() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.fail(Fault::ObjectList);

        store
            .obliterate(
                repository.into(),
                address,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect_err("listing versions fails");

        assert_ne!(
            fake.state_of(address.hash),
            Some(FragmentState::Obliterated)
        );
    }

    /// Recursion must not report success when a sub-fragment could not be obliterated, or the
    /// parent would be tombstoned over content that is still referenced.
    #[tokio::test]
    async fn obliterate_fails_when_a_sub_fragment_fails() {
        let fake = Fake::default();
        let repository: Context = random();
        let context: Context = random();
        let leaves = [random::<Hash>(), random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, repository, context, &leaves).await;

        fake.fail(Fault::ObjectDelete);

        store
            .obliterate(
                repository.into(),
                parent,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect_err("a sub-fragment failure must fail the parent");

        assert_ne!(fake.state_of(parent.hash), Some(FragmentState::Obliterated));
    }

    /// The repair is best effort: failing to clear the row must not turn a not-found into an error,
    /// because the caller's answer does not depend on the repair succeeding.
    #[tokio::test]
    async fn a_failed_repair_still_reports_the_lost_payload_as_not_found() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);
        fake.fail(Fault::StateDelete);

        let error = store
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("the payload is gone");

        assert!(error.is_address_not_found());
        assert_eq!(
            fake.state_of(address.hash),
            Some(FragmentState::Stored),
            "the row survives a failed repair, unchanged"
        );
    }

    /// The probe saw nothing, the upload landed, and an obliteration took the hash in between. The
    /// put must not associate, or it would restore a reference the obliteration is removing.
    #[tokio::test]
    async fn a_put_that_loses_the_hash_mid_upload_backs_off_without_associating() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.obliterate_during_upload(address.hash, FragmentState::Obliterating);

        let error = store(&fake)
            .await
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect_err("an obliteration holds the hash by the time the put publishes");

        assert!(
            error.is_slow_down(),
            "the mark is transient, so this is a back-off"
        );
        assert_eq!(
            fake.association_count(address.hash),
            0,
            "no reference may be created while an obliteration holds the hash"
        );
        assert_eq!(
            fake.state_of(address.hash),
            Some(FragmentState::Obliterating),
            "the put must not disturb the mark"
        );
    }

    /// Racing a *completed* obliteration is different: the tombstone is not a lock, and re-upload
    /// over one is allowed, so the put finishes and revives the hash.
    #[tokio::test]
    async fn a_put_that_lands_on_a_fresh_tombstone_revives_it() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.obliterate_during_upload(address.hash, FragmentState::Obliterated);

        store(&fake)
            .await
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("re-upload over a tombstone is allowed");

        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        assert_eq!(fake.association_count(address.hash), 1);
    }

    /// The drain exists so a put that had already passed its state probe can land its association
    /// and be counted. Without the wait the count runs immediately, sees nothing, and the payload
    /// is deleted underneath a partition that legitimately stored it.
    #[tokio::test]
    async fn the_drain_lets_an_in_flight_association_be_counted() {
        let fake = Fake::default();
        let hash: Hash = random();
        let mine = Address {
            hash,
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), mine, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let deleted = fake.association_deleted();
        let injector = fake.clone();
        let racing_repository: Context = random();
        let racing = Address {
            hash,
            context: random(),
        };
        let mut tasks = JoinSet::new();
        lore_base::lore_spawn!(tasks, async move {
            deleted
                .await
                .expect("the obliteration must delete its association");
            injector.add_association(racing_repository, racing);
        });

        store
            .obliterate(
                repository.into(),
                mine,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate should succeed");

        while let Some(result) = tasks.join_next().await {
            result.expect("the racing writer should not panic");
        }

        assert!(
            fake.object(hash).is_some(),
            "an association that landed during the drain must keep the payload alive"
        );
        assert_eq!(fake.association_count(hash), 1);
        assert_eq!(
            fake.state_of(hash),
            Some(FragmentState::Stored),
            "the mark must be released so the surviving reference stays usable"
        );
    }

    // ---------------------------------------------------------------------
    // Surface that had coverage on main
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn copy_associates_the_destination_without_touching_the_payload() {
        let fake = Fake::default();
        let source = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(
                repository.into(),
                source,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put should succeed");

        let destination_context: Context = random();
        store
            .copy(
                repository.into(),
                source,
                repository.into(),
                destination_context,
                true,
            )
            .await
            .expect("copy should succeed");

        assert_eq!(fake.association_count(source.hash), 2);
        assert_eq!(
            fake.object(source.hash).unwrap().0,
            payload.as_ref(),
            "copy must not rewrite the payload"
        );
        assert_eq!(fake.object_reads(), 0, "copy must not read S3");
    }

    #[tokio::test]
    async fn copy_of_an_unknown_address_is_not_found() {
        let fake = Fake::default();
        let source = Address {
            hash: random(),
            context: random(),
        };
        let repository: Context = random();

        store(&fake)
            .await
            .copy(
                repository.into(),
                source,
                repository.into(),
                random::<Context>(),
                true,
            )
            .await
            .expect_err("nothing to copy");

        assert_eq!(fake.association_count(source.hash), 0);
    }

    /// A hash present in another context is not a full match, so it must not be copyable from this
    /// one — the copy would otherwise fabricate a reference from a partial match.
    #[tokio::test]
    async fn copy_of_a_partial_match_is_not_found() {
        let fake = Fake::default();
        let hash: Hash = random();
        let stored = Address {
            hash,
            context: random(),
        };
        let repository: Context = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(repository.into(), stored, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let other_context = Address {
            hash,
            context: random(),
        };
        store
            .copy(
                repository.into(),
                other_context,
                repository.into(),
                random::<Context>(),
                true,
            )
            .await
            .expect_err("a different context is not a full match");

        assert_eq!(fake.association_count(hash), 1);
    }

    #[tokio::test]
    async fn exist_batch_reports_a_match_per_address_in_order() {
        let fake = Fake::default();
        let repository: Context = random();
        let absent = Address {
            hash: random(),
            context: random(),
        };
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        let mut stored = Vec::new();
        for _ in 0..2 {
            let address = Address {
                hash: random(),
                context: random(),
            };
            store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("put should succeed");
            stored.push(address);
        }

        let results = store
            .exist_batch(
                repository.into(),
                &[stored[0], absent, stored[1]],
                StoreMatch::MatchFull,
            )
            .await
            .expect("exist_batch should succeed");

        assert_eq!(
            results,
            vec![
                StoreMatch::MatchFull,
                StoreMatch::MatchNone,
                StoreMatch::MatchFull
            ],
            "results must line up with the addresses given, misses included"
        );
    }

    /// The corruption this design exists to make impossible.
    ///
    /// Writers race on one hash with different representations, each a valid encoding of the same
    /// content. Under a model that stores the fragment separately from the bytes, an interleaving
    /// can leave one writer's fragment describing another writer's payload. Here the fragment
    /// travels on the object, so whichever upload lands last is the one that is read back — whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_cannot_tear_the_fragment_from_its_payload() {
        const CONTENT_SIZE: u64 = 4096;

        for _ in 0..64 {
            let fake = Fake::default();
            let hash: Hash = random();
            let store = store(&fake).await;

            let representations = [
                representation(FragmentFlags::PayloadCompressedZstd, 64, CONTENT_SIZE),
                representation(FragmentFlags::PayloadCompressedLZ4, 512, CONTENT_SIZE),
                representation(FragmentFlags::PayloadCompressedOodle2, 1024, CONTENT_SIZE),
                representation(FragmentFlags::PayloadFragmented, 2048, CONTENT_SIZE),
            ];

            let mut writers = JoinSet::new();
            for (fragment, payload) in representations {
                let store = store.clone();
                let address = Address {
                    hash,
                    context: random(),
                };
                let repository: Context = random();

                lore_base::lore_spawn!(writers, async move {
                    store
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                });
            }

            while let Some(result) = writers.join_next().await {
                result
                    .expect("writer task should not panic")
                    .expect("every writer should succeed");
            }

            let (body, metadata) = fake.object(hash).expect("a payload must be stored");
            let stored = from_object_metadata(Some(&metadata))
                .expect("the stored object must carry a fragment");

            assert_eq!(
                stored.size_payload as usize,
                body.len(),
                "the fragment on the object must describe the bytes on that same object"
            );
            assert_eq!(stored.size_content, CONTENT_SIZE);
            assert_eq!(
                fake.state_of(hash),
                Some(FragmentState::Stored),
                "the state row must not be left mid-flight"
            );
            assert_eq!(
                fake.association_count(hash),
                4,
                "every writer's partition must end up referencing the payload"
            );

            let expected_byte = (stored.flags & PAYLOAD_FLAGS) as u8;
            assert!(
                body.iter().all(|byte| *byte == expected_byte),
                "the bytes must be the ones the winning writer uploaded, not a mix"
            );
        }
    }
}
