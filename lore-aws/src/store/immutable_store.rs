// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
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
use lore_storage::ImmutableStore as ImmutableStoreTrait;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
#[cfg(test)]
use lore_storage::immutable_store::query_one;
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

enum QueryResultSource {
    LegacyMetadata(Fragment),
    State,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct FragmentsEntry {
    hash: Hash,
    /// The partition that holds the association, followed by the address's context. Stored under
    /// its original attribute name, which predates the split between the two.
    #[serde(with = "serde_bytes", rename = "repository_context")]
    partition_context: [u8; size_of::<Context>() * 2],
}

impl From<&FragmentsEntry> for Address {
    fn from(value: &FragmentsEntry) -> Self {
        Address {
            hash: value.hash,
            context: Context::from(&value.partition_context[size_of::<Context>()..]),
        }
    }
}

impl Debug for FragmentsEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FragmentsEntry")
            .field("hash", &self.hash)
            .field("partition_context", &hex::encode(self.partition_context))
            .finish()
    }
}

impl FragmentsEntry {
    fn new(partition: Partition, address: Address) -> Self {
        let mut partition_context = [0u8; size_of::<Context>() * 2];
        partition_context[..size_of::<Context>()].copy_from_slice(partition.data());
        partition_context[size_of::<Context>()..].copy_from_slice(address.context.data());

        Self {
            hash: address.hash,
            partition_context,
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
enum FragmentState {
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
struct FragmentStateEntry {
    hash: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<u32>,
}

/// A row in the shape written before fragments moved onto the S3 object: the whole fragment,
/// flattened alongside the hash.
///
/// Deserialization only. Nothing writes this shape any more.
#[derive(Clone, Debug, Deserialize)]
struct FragmentMetadataEntry {
    hash: Hash,
    #[serde(flatten)]
    fragment: Option<Fragment>,
}

impl FragmentStateEntry {
    fn key(hash: Hash) -> Self {
        Self { hash, state: None }
    }

    fn new(hash: Hash, state: FragmentState) -> Self {
        Self {
            hash,
            state: Some(state.bits()),
        }
    }

    fn state(&self) -> FragmentState {
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
#[derive(Clone, Debug, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct AwsImmutableStoreSettings {
    pub s3: S3StoreSettings,
    pub dynamodb: DynamoDbImmutableStoreSettings,
    #[serde(default)]
    pub force_write: bool,
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
        }
    }
}

pub const FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE: &str = "hash";
pub const FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE: &str = "repository_context";

/// How many associations exist for a hash, across every partition.
///
/// The only `Query` this store issues. Everything else it asks is a keyed read: it reads no wider
/// than the exact association, so there is nothing to scan a hash partition for except counting
/// the references that keep a payload alive.
#[derive(Debug, Clone, PartialEq)]
enum FragmentsQuery {
    HashCount(Hash),
}

impl DynamoDbQuery for FragmentsQuery {
    fn key_condition_expression(&self) -> &str {
        "#pk = :hash"
    }

    fn expression_attribute_names(&self) -> HashMap<String, String> {
        HashMap::from([(
            "#pk".to_string(),
            FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
        )])
    }

    fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
        let FragmentsQuery::HashCount(hash) = self;
        HashMap::from([(
            ":hash".to_string(),
            AttributeValue::B(Blob::new(hash.data())),
        )])
    }

    fn limit(&self) -> Option<i32> {
        None
    }

    fn select(&self) -> Option<Select> {
        Some(Select::Count)
    }

    fn consistent_read(&self) -> bool {
        true
    }
}

/// Write only if no row exists for this hash yet.
///
/// Publishing a payload uses this so that a concurrent obliteration's mark cannot be erased by a
/// racing writer: the writer's create loses, it re-reads the row, and it sees the mark.
#[derive(Debug, PartialEq)]
struct RowAbsent;

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
struct StateUnchanged(FragmentState);

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

/// Counts reads that found an association whose hash has no lifecycle state recorded anywhere.
///
/// A put publishes the state before it writes the association, so a correctly written fragment
/// always has one. Content stored before the state table existed does not, and is read from the
/// legacy metadata table instead — which is only consulted when that table is configured. So a
/// non-zero count means either a deployment holding legacy content without
/// `fragment_metadata_table_name` set, or a put that stopped between publishing and associating.
/// The read reports absence either way, so without this the cause is invisible.
const METRICS_ASSOCIATION_WITHOUT_STATE_METRIC_NAME: &str =
    "store.immutable.association_without_state";

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
    labels_obliterate: LabelArray,
    labels_copy: LabelArray,
    labels_get_metadata: LabelArray,
    missing_payload_counter: Counter<u64>,
    labels_missing_payload: LabelArray,
    association_without_state_counter: Counter<u64>,
    labels_association_without_state: LabelArray,
}

impl AwsImmutableStore {
    pub fn new(s3: S3, dynamodb: DynamoDb, settings: &AwsImmutableStoreSettings) -> Self {
        let provider = AwsImmutableStoreInstrumentProvider;

        let latency_histogram =
            provider.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME);
        let labels_get = provider.get_labels_for_operation_context("get");
        let labels_put = provider.get_labels_for_operation_context("put");
        let labels_obliterate = provider.get_labels_for_operation_context("obliterate");
        let labels_copy = provider.get_labels_for_operation_context("copy");
        let labels_get_metadata = provider.get_labels_for_operation_context("get_metadata");
        let missing_payload_counter = provider.counter(METRICS_MISSING_PAYLOAD_METRIC_NAME);
        let labels_missing_payload = provider.get_labels_for_operation_context("missing_payload");
        let association_without_state_counter =
            provider.counter(METRICS_ASSOCIATION_WITHOUT_STATE_METRIC_NAME);
        let labels_association_without_state =
            provider.get_labels_for_operation_context("association_without_state");
        Self {
            s3,
            dynamodb,
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
            labels_obliterate,
            labels_copy,
            labels_get_metadata,
            missing_payload_counter,
            labels_missing_payload,
            association_without_state_counter,
            labels_association_without_state,
        }
    }

    /// Whether this partition holds the association for this address, which is the whole of what
    /// this store will serve: it isolates partitions, so it reads no wider than the association
    /// asked about.
    async fn exists(&self, partition: Partition, address: Address) -> Result<bool, StoreError> {
        let entry = FragmentsEntry::new(partition, address);
        let item = serde_dynamo::to_item(&entry).map_err(|e| {
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

    /// The level this address matched at, or `AddressNotFound` when nothing within the store's read
    /// scope does. One call: the read scope is the exact association, so a miss is the answer
    /// rather than the start of a wider search.
    /// The addresses among `addresses` that this partition holds an association for.
    ///
    /// A set rather than positions: a batch may name the same address more than once, and keying
    /// positions by address loses every occurrence but the last. `BatchGetItem` also rejects
    /// duplicate keys outright, so the request is built from distinct addresses regardless.
    async fn associations_present(
        &self,
        partition: Partition,
        addresses: &[Address],
    ) -> Result<HashSet<Address>, StoreError> {
        let distinct: HashSet<Address> = addresses.iter().copied().collect();
        if distinct.is_empty() {
            return Ok(HashSet::new());
        }

        let mut items = Vec::with_capacity(distinct.len());
        for address in &distinct {
            let entry = FragmentsEntry::new(partition, *address);
            items.push(serde_dynamo::to_item(&entry).map_err(|e| {
                warn!("Failed to serialize fragment entry {entry:?} for resolve: {e:?}");
                StoreError::internal_with_context(e, "Failed to serialize fragment entry")
            })?);
        }

        let output = self
            .dynamodb
            .batch_get_item(&self.fragments_table_name, items, true)
            .await
            .map_err(|e| {
                warn!("DynamoDb association resolve failed: {e:?}");
                if is_dynamodb_overloaded(&e) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB batch get items failed")
                }
            })?;

        let mut present = HashSet::with_capacity(output.len());
        for item in output {
            match serde_dynamo::from_item::<HashMap<String, AttributeValue>, FragmentsEntry>(item) {
                Ok(entry) => {
                    present.insert((&entry).into());
                }
                Err(e) => warn!("Failed to convert dynamo item to fragments entry: {e:?}"),
            }
        }

        Ok(present)
    }

    /// A full match or nothing. This store reads no wider than the exact association, so there is
    /// no weaker level for a miss to fall back to.
    /// Resolve an address to the fragment stored for it, without transferring the payload.
    ///
    /// An obliterated hash needs no special case: obliteration deletes the object, so the head
    /// returns not-found and the query reports a miss. Between an obliteration taking its mark and
    /// deleting the association there is a window where a partition that still holds a reference
    /// sees the fragment — which is accurate, since the payload survives for as long as any
    /// reference to it does.
    async fn do_query(
        &self,
        partition: Partition,
        address: Address,
    ) -> Result<(QueryResultSource, StoreGetData), StoreError> {
        let (associated, state) = tokio::join!(
            self.exists(partition, address),
            self.load_state(address.hash)
        );

        let miss = Ok((QueryResultSource::State, StoreGetData::default()));

        if !associated? {
            return miss;
        }

        let match_made = StoreMatch::MatchFull;

        match state? {
            Some(FragmentState::Stored) => Ok((
                QueryResultSource::State,
                StoreGetData::metadata(stored_durable(Fragment::default()), match_made, partition),
            )),
            Some(FragmentState::Obliterating | FragmentState::Obliterated) => {
                trace!("Query found obliterated fragment at address {address}");
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
                                StoreGetData::metadata(fragment, match_made, partition),
                            ))
                        }
                        FragmentState::Obliterating | FragmentState::Obliterated => {
                            trace!("Query found obliterated legacy fragment at address {address}");
                            miss
                        }
                    };
                }

                self.association_without_state_counter
                    .add(1, &self.labels_association_without_state);
                trace!("Query found an association at {address} with no stored payload");
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
        partition: Partition,
        address: Address,
    ) -> Result<(), StoreError> {
        let entry = FragmentsEntry::new(partition, address);

        let item = serde_dynamo::to_item(&entry).map_err(|e| {
            warn!("Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment association for DynamoDB",
            )
        })?;

        self.dynamodb.put_item(&self.fragments_table_name, item).await
            .map_err(|e| {
                warn!({REPOSITORY_ID} = %partition, {ADDRESS} = %address, error = ?e, "Failed to put item while storing fragment association");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB fragment association write failed")
                }
            })?;

        Ok(())
    }

    /// The lifecycle state of each distinct hash among `addresses`, where a row exists.
    ///
    /// One request for the batch rather than one per hash: the state table is keyed by hash alone,
    /// which is the whole reason it is a separate table.
    async fn states_for(
        &self,
        addresses: &[Address],
    ) -> Result<HashMap<Hash, FragmentState>, StoreError> {
        let distinct: HashSet<Hash> = addresses.iter().map(|address| address.hash).collect();
        if distinct.is_empty() {
            return Ok(HashMap::new());
        }

        let mut items = Vec::with_capacity(distinct.len());
        for hash in &distinct {
            items.push(
                serde_dynamo::to_item(FragmentStateEntry::key(*hash)).map_err(|e| {
                    warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
                    StoreError::internal_with_context(e, "Failed to serialize fragment state entry")
                })?,
            );
        }

        let output = self
            .dynamodb
            .batch_get_item(&self.fragment_state_table_name, items, true)
            .await
            .map_err(|e| {
                warn!("DynamoDb fragment state resolve failed: {e:?}");
                if is_dynamodb_overloaded(&e) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB batch get items failed")
                }
            })?;

        let mut states = HashMap::with_capacity(output.len());
        for item in output {
            match serde_dynamo::from_item::<HashMap<String, AttributeValue>, FragmentStateEntry>(
                item,
            ) {
                Ok(entry) => {
                    states.insert(entry.hash, entry.state());
                }
                Err(e) => warn!("Failed to convert dynamo item to fragment state entry: {e:?}"),
            }
        }

        Ok(states)
    }

    /// The state of each hash that still lives only in the legacy metadata table, read in one
    /// request rather than one per hash.
    ///
    /// A fragment written before the state table existed has no state row, and its lifecycle lives
    /// in the flags of its metadata row instead. Without this an address stored in that era resolves
    /// to absence, and a push would ask a client to upload content it does not have and the store
    /// already holds.
    async fn legacy_states_for(
        &self,
        hashes: &[Hash],
    ) -> Result<HashMap<Hash, FragmentState>, StoreError> {
        let Some(table_name) = self.fragment_metadata_table_name.as_ref() else {
            return Ok(HashMap::new());
        };

        // `BatchGetItem` rejects a request carrying the same key twice, and a batch may well name
        // one hash under two contexts.
        let distinct: HashSet<Hash> = hashes.iter().copied().collect();
        if distinct.is_empty() {
            return Ok(HashMap::new());
        }

        let mut items = Vec::with_capacity(distinct.len());
        for hash in &distinct {
            items.push(
                serde_dynamo::to_item(FragmentStateEntry::key(*hash)).map_err(|e| {
                    warn!("Failed to serialize legacy fragment key for {hash}: {e:?}");
                    StoreError::internal_with_context(
                        e,
                        "Failed to serialize fragment entry for legacy metadata load",
                    )
                })?,
            );
        }

        let output = self
            .dynamodb
            .batch_get_item(table_name, items, true /* consistent read */)
            .await
            .map_err(|e| {
                warn!("DynamoDb legacy fragment metadata batch read failed: {e:?}");
                if is_dynamodb_overloaded(&e) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(e, "DynamoDB batch get items failed")
                }
            })?;

        let mut states = HashMap::with_capacity(output.len());
        for item in output {
            match serde_dynamo::from_item::<HashMap<String, AttributeValue>, FragmentMetadataEntry>(
                item,
            ) {
                Ok(entry) => {
                    if let Some(fragment) = entry.fragment {
                        states.insert(entry.hash, FragmentState::from_bits(fragment.flags));
                    }
                }
                Err(e) => warn!("Failed to convert dynamo item to legacy metadata entry: {e:?}"),
            }
        }

        Ok(states)
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
        partition: Partition,
        address: Address,
    ) -> Result<(), StoreError> {
        let entry = FragmentsEntry::new(partition, address);

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
                warn!("Failed to delete fragment association for partition: {partition} and address: {address}: {e:?}");
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
        partition: Partition,
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
            .put_object(
                self.bucket.as_str(),
                hash,
                payload,
                Some(to_object_metadata(&fragment)),
            )
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

        match self.publish_state(address.hash).await? {
            FragmentState::Stored => {}
            FragmentState::Obliterating => {
                info!(
                    "Payload for {address} was uploaded while an obliteration holds the hash; \
                     leaving it unassociated and asking the caller to retry"
                );
                return Err(StoreError::from(SlowDown));
            }
            FragmentState::Obliterated => {
                info!("Payload for {address} revives a tombstoned hash");
                self.revive_state(address.hash).await?;
            }
        }

        self.associate_fragment(partition, address).await?;

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
                    .map(|versions| versions.into_iter().map(|v| v.version_id).collect())
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
    async fn load_state(&self, hash: Hash) -> Result<Option<FragmentState>, StoreError> {
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

        // Clamped because the length is the response's claim, and a fragment cannot exceed the
        // threshold.
        let capacity = output
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .filter(|length| *length > 0)
            .map_or(FRAGMENT_SIZE_THRESHOLD, |length| {
                length.min(FRAGMENT_SIZE_THRESHOLD)
            });

        let mut buffer = BytesMut::with_capacity(capacity);
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
    async fn load(&self, hash: Hash) -> Result<(Fragment, Bytes), StoreError> {
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
        partition: Partition,
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
                        .obliterate(partition, sub_address, stats)
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
    /// Durable storage for every tenant at once, so a payload is only ever served to the partition
    /// that holds an association for it.
    fn isolates_partitions(&self) -> bool {
        true
    }

    /// Two batch reads, issued together: the associations this partition holds, and the lifecycle
    /// state of each hash. What they yield resolves without a third:
    ///
    /// | state | association | reported |
    /// |---|---|---|
    /// | not `Stored` | either | `MatchNone` |
    /// | `Stored` | present | `MatchFull` |
    /// | `Stored` | absent | `MatchNone` |
    ///
    /// The associations alone carry that much because of how they are written: a put adds the
    /// association last, after the payload and the state are in place, and an obliteration deletes
    /// it first. A visible association therefore already means retrievable content.
    ///
    /// The last row reports less than the truth rather than claiming the partition holds no such
    /// hash. Telling an unassociated hash apart from an absent one costs a `Query` per address, and
    /// this store declines to spend it — so it never reports `MatchPartition` either. A caller
    /// reads the weaker answer as "no shortcut available", and in the usual deployment a cache in
    /// front of this store supplies the partition match instead.
    ///
    /// State is read only while the legacy metadata table is configured, and it is what makes
    /// obliteration bind for content written before the state table existed.
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "AwsImmutableStore::resolve" skip(self))]
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        // Neither read needs what the other returns.
        let (associations, states) = if self.fragment_metadata_table_name.is_some() {
            let (associations, states) = tokio::join!(
                self.associations_present(partition, addresses),
                self.states_for(addresses)
            );
            (associations?, Some(states?))
        } else {
            (self.associations_present(partition, addresses).await?, None)
        };

        // This one has to wait: which hashes it asks about is whatever the two reads above left
        // unresolved.
        let legacy_states = match states.as_ref() {
            Some(states) => {
                let pending: Vec<Hash> = addresses
                    .iter()
                    .filter(|address| {
                        associations.contains(address) && !states.contains_key(&address.hash)
                    })
                    .map(|address| address.hash)
                    .collect();
                self.legacy_states_for(&pending).await?
            }
            None => HashMap::new(),
        };

        for (address, result) in addresses.iter().zip(results.iter_mut()) {
            if !associations.contains(address) {
                *result = StoreMatchResult::default();
                continue;
            }

            if let Some(states) = states.as_ref() {
                let state = states
                    .get(&address.hash)
                    .or_else(|| legacy_states.get(&address.hash))
                    .copied();

                if !matches!(state, Some(FragmentState::Stored)) {
                    *result = StoreMatchResult::default();
                    continue;
                }
            }

            *result = StoreMatchResult {
                match_made: StoreMatch::MatchFull,
                // This store isolates, so it only ever matches inside the partition asked about.
                partition,
                stored_local: false,
                stored_durable: true,
            };
        }

        Ok(())
    }

    /// Unlike [`AwsImmutableStore::resolve`], this reads the object to report the representation
    /// actually stored, which costs a `HeadObject`. It transfers no body, and it is the only path
    /// in this store that spends an S3 request purely on metadata.
    #[lore_macro::lore_instrument]
    #[tracing::instrument(name = "AwsImmutableStore::get_metadata" skip(self))]
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        timed!(self.latency_histogram, &self.labels_get_metadata, {
            let miss = StoreGetData::default();

            // One resolution, because this store's read scope is the exact association: there is
            // no wider level for a miss to fall back to.
            let (query_source, query_result) = Box::pin(self.do_query(partition, address)).await?;
            let match_made = query_result.match_made;

            if match_made == StoreMatch::MatchNone {
                return Ok(miss);
            }

            match query_source {
                // A legacy fragment carried its whole representation in the metadata row, so the
                // `HeadObject` that would describe it has nothing to add and no object to read.
                QueryResultSource::LegacyMetadata(fragment) => {
                    Ok(StoreGetData::metadata(fragment, match_made, partition))
                }
                QueryResultSource::State => match self.head_fragment(address.hash).await {
                    Ok(fragment) => Ok(StoreGetData::metadata(fragment, match_made, partition)),
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
    ) -> Result<StoreGetData, StoreError> {
        let result: Result<(Fragment, Bytes), StoreError> =
            timed!(self.latency_histogram, &self.labels_get, {
                // Run both futures concurrently. The select! loop breaks as soon as exists resolves.
                // If load finishes first its result is stashed, and we keep waiting for exists check.
                let exists_fut = self.exists(partition, address);
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
                if !exists_result? {
                    return Err(StoreError::from(AddressNotFound::from(address)));
                }

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
        Ok(StoreGetData {
            fragment,
            match_made: StoreMatch::MatchFull,
            partition,
            payload: Some(payload),
        })
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
        timed!(self.latency_histogram, &self.labels_put, {
            let probe = if self.force_write {
                (None, false)
            } else {
                let (associated, state) = tokio::join!(
                    self.exists(partition, address),
                    self.load_state(address.hash)
                );
                (state?, associated?)
            };

            match probe {
                (Some(FragmentState::Obliterating), _) => {
                    debug!(
                        "Received request to put fragment at {address} that is in the process of \
                         being obliterated"
                    );
                    Err(StoreError::from(SlowDown))
                }

                (Some(FragmentState::Stored), true) => Ok(()),

                (Some(FragmentState::Stored), false) if payload.is_some() => {
                    self.associate_fragment(partition, address).await
                }

                (Some(FragmentState::Stored), false) => {
                    Err(StoreError::internal("Payload buffer required"))
                }

                _ => match payload {
                    Some(payload) => {
                        self.write_payload(partition, address, fragment, payload)
                            .await
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
        timed!(self.latency_histogram, &self.labels_obliterate, {
            // Note: given the importance of the work done here, and how relatively infrequently we
            // expect this to be invoked, the log output in this method is intentionally very verbose.
            let span = tracing::Span::current();

            // Content written before the state table existed has no row here, so this returns
            // having deleted nothing: the association stays, and still resolves to a full match.
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

            // The reference goes first, because that is the obligation: once this returns, this
            // partition does not name the content. Everything after it is reclamation.
            self.delete_association(partition, address)
                .instrument(span.clone())
                .await?;
            stats
                .num_fragments
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            self.advance_state(
                address.hash,
                FragmentState::Stored,
                FragmentState::Obliterating,
            )
            .instrument(span.clone())
            .await?;
            info!("Acquired obliteration mark for {address}");

            // Again, now the mark is up: a writer that raced the first delete recreated the
            // association while the hash still looked live, and would otherwise outlive the
            // obliteration. One that associates after this point sees the mark and is turned away,
            // or is re-storing content of its own - which is a new write, not this one surviving.
            self.delete_association(partition, address)
                .instrument(span.clone())
                .await?;

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
                .obliterate_sub_fragments(partition, address, stats.clone())
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
        // The destination tuple shares the source's hash but takes the caller's chosen context
        // — that is the only field the storage trait allows the caller to pivot on a copy.
        let destination_address = Address {
            hash: source_address.hash,
            context: destination_context,
        };
        timed!(self.latency_histogram, &self.labels_copy, {
            if !self.exists(source_partition, source_address).await? {
                return Err(StoreError::from(AddressNotFound::from(source_address)));
            }

            self.associate_fragment(destination_partition, destination_address)
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
    use std::collections::HashSet;
    use std::sync::Mutex;
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
    use aws_sdk_s3::operation::head_object::HeadObjectOutput;
    use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsError;
    use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsOutput;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::primitives::SdkBody;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_sdk_s3::types::error::NotFound;
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::client::result::SdkError;
    use aws_smithy_runtime_api::client::result::ServiceError;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::FragmentFlags;
    use lore_storage::ImmutableStore;
    use rand::random;
    use tokio::sync::oneshot;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::dynamodb::MockDynamoDb;
    use crate::s3::MockS3Impl;
    use crate::store::object_metadata::PAYLOAD_FLAGS;
    use crate::store::setup_execution;

    const BUCKET: &str = "test-bucket";
    const FRAGMENTS_TABLE_NAME: &str = "fragments";
    const FRAGMENT_STATE_TABLE_NAME: &str = "fragment-state";
    /// A separate table name for legacy fragment metadata, distinct from the state table. Used
    /// to test the `do_query` path that falls back to the metadata table when no state row exists.
    const FRAGMENT_METADATA_TABLE_NAME: &str = "fragment-metadata";

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

    fn blob(item: &HashMap<String, AttributeValue>, key: &str) -> Vec<u8> {
        item.get(key)
            .and_then(|value| value.as_b().ok())
            .map(|value| value.as_ref().to_vec())
            .unwrap_or_default()
    }

    /// An in-memory stand-in for the bucket and the two tables.
    ///
    /// The tests are written against behaviour rather than a call sequence: they put and get
    /// through the real store and assert on what ends up stored. That is what lets the concurrency
    /// test exist at all — a mock programmed with an expected order of calls cannot express
    /// "any interleaving, and the result must still be coherent".
    /// A stored object: its body, and the object metadata written with it.
    type StoredObject = (Vec<u8>, HashMap<String, String>);

    /// An operation the fake can be told to fail, so error paths are reachable.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    enum Fault {
        StateRead,
        StateReadTimeout,
        StateReadBroken,
        StateWrite,
        StateDelete,
        AssociationWrite,
        AssociationDelete,
        AssociationCount,
        ObjectDelete,
        ObjectList,
    }

    #[derive(Default)]
    struct Storage {
        faults: HashSet<Fault>,
        race_state: Option<(Hash, FragmentState)>,
        /// Fired when an obliteration deletes its association, so a task can land one of its own
        /// in the window that follows.
        association_deleted: Option<oneshot::Sender<()>>,
        object_reads: usize,
        objects: HashMap<Vec<u8>, StoredObject>,
        associations: HashMap<(Vec<u8>, Vec<u8>), HashMap<String, AttributeValue>>,
        state: HashMap<Vec<u8>, HashMap<String, AttributeValue>>,
        /// Rows in the legacy fragment metadata table (separate from the state table). Only
        /// populated by `set_legacy_metadata_row`, which is used by tests that need to exercise
        /// the `do_query` path where no state row exists but a legacy metadata row does.
        legacy_metadata: HashMap<Vec<u8>, HashMap<String, AttributeValue>>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Storage>>);

    impl Fake {
        fn lock(&self) -> std::sync::MutexGuard<'_, Storage> {
            self.0.lock().unwrap()
        }

        /// Make `fault` fail from now on. Latched rather than one-shot, so a retrying caller sees a
        /// persistent failure rather than one that heals underneath it.
        fn fail(&self, fault: Fault) {
            self.lock().faults.insert(fault);
        }

        /// Move `hash` into `state` at the moment its payload is uploaded, so a put reaches its
        /// publish step having probed before an obliteration and uploaded after it. That window
        /// cannot be hit by ordering calls from the outside.
        fn obliterate_during_upload(&self, hash: Hash, state: FragmentState) {
            self.lock().race_state = Some((hash, state));
        }

        fn failing(&self, fault: Fault) -> bool {
            self.lock().faults.contains(&fault)
        }

        fn object_reads(&self) -> usize {
            self.lock().object_reads
        }

        fn object(&self, hash: Hash) -> Option<StoredObject> {
            self.lock()
                .objects
                .get(&hash.to_string().into_bytes())
                .cloned()
        }

        fn stored_fragment(&self, hash: Hash) -> Option<Fragment> {
            self.object(hash)
                .map(|(_, metadata)| from_object_metadata(Some(&metadata)).unwrap())
        }

        fn state_of(&self, hash: Hash) -> Option<FragmentState> {
            self.lock()
                .state
                .get(hash.data().as_slice())
                .map(|item| serde_dynamo::from_item::<_, FragmentStateEntry>(item.clone()).unwrap())
                .map(|entry| entry.state())
        }

        fn association_count(&self, hash: Hash) -> usize {
            self.lock()
                .associations
                .keys()
                .filter(|(stored, _)| stored == hash.data())
                .count()
        }

        fn set_state(&self, hash: Hash, state: FragmentState) {
            let item = serde_dynamo::to_item(FragmentStateEntry::new(hash, state)).unwrap();
            self.lock().state.insert(hash.data().to_vec(), item);
        }

        /// Write a row in the shape used before fragments moved onto the object: no `state`, and a
        /// whole flattened fragment whose `flags` also carry the obliteration bits.
        fn set_fragment_metadata_row(&self, hash: Hash, fragment: Fragment) {
            let item = HashMap::from([
                (
                    "hash".to_owned(),
                    AttributeValue::B(Blob::new(hash.data().to_vec())),
                ),
                (
                    "flags".to_owned(),
                    AttributeValue::N(fragment.flags.to_string()),
                ),
                (
                    "size_payload".to_owned(),
                    AttributeValue::N(fragment.size_payload.to_string()),
                ),
                (
                    "size_content".to_owned(),
                    AttributeValue::N(fragment.size_content.to_string()),
                ),
            ]);

            self.lock().state.insert(hash.data().to_vec(), item);
        }

        /// Write a legacy fragment metadata row into the *separate* metadata table (keyed by
        /// `FRAGMENT_METADATA_TABLE_NAME`). Unlike `set_fragment_metadata_row`, this does NOT
        /// touch `storage.state`, so `load_state` returns `None` for the same hash, letting tests
        /// exercise the `do_query` branch that falls back to the metadata table when there is no
        /// state row.
        fn set_legacy_metadata_row(&self, hash: Hash, fragment: Fragment) {
            let item = HashMap::from([
                (
                    "hash".to_owned(),
                    AttributeValue::B(Blob::new(hash.data().to_vec())),
                ),
                (
                    "flags".to_owned(),
                    AttributeValue::N(fragment.flags.to_string()),
                ),
                (
                    "size_payload".to_owned(),
                    AttributeValue::N(fragment.size_payload.to_string()),
                ),
                (
                    "size_content".to_owned(),
                    AttributeValue::N(fragment.size_content.to_string()),
                ),
            ]);

            self.lock()
                .legacy_metadata
                .insert(hash.data().to_vec(), item);
        }

        /// Delete an object while leaving every reference to it in place, as an obliteration
        /// interrupted before its tombstone or an S3 durability event would.
        fn lose_object(&self, hash: Hash) {
            self.lock().objects.remove(&hash.to_string().into_bytes());
        }

        /// Store an object the way one was stored before the fragment moved onto it: bare bytes,
        /// no fragment metadata.
        fn put_object_without_metadata(&self, hash: Hash, body: &[u8]) {
            self.lock().objects.insert(
                hash.to_string().into_bytes(),
                (body.to_vec(), HashMap::new()),
            );
        }

        /// Store an object whose metadata is present but unreadable.
        fn put_object_with_damaged_metadata(&self, hash: Hash, body: &[u8]) {
            let mut metadata = HashMap::new();
            metadata.insert("lore-fragment".to_owned(), "not:a:fragment".to_owned());

            self.lock()
                .objects
                .insert(hash.to_string().into_bytes(), (body.to_vec(), metadata));
        }

        /// Signals when an obliteration deletes its association, so a caller can land its own
        /// between that delete and the re-count — the window the drain exists to cover. Ordering
        /// calls from the outside cannot hit it.
        fn association_deleted(&self) -> oneshot::Receiver<()> {
            let (sender, receiver) = oneshot::channel();
            self.lock().association_deleted = Some(sender);
            receiver
        }

        fn has_association(&self, partition: Partition, address: Address) -> bool {
            let entry = FragmentsEntry::new(partition, address);
            self.lock()
                .associations
                .contains_key(&(entry.hash.data().to_vec(), entry.partition_context.to_vec()))
        }

        fn add_association(&self, partition: Partition, address: Address) {
            let entry = FragmentsEntry::new(partition, address);
            let item: HashMap<String, AttributeValue> = serde_dynamo::to_item(&entry).unwrap();
            self.lock().associations.insert(
                (entry.hash.data().to_vec(), entry.partition_context.to_vec()),
                item,
            );
        }
    }

    /// Wire the fake into the generated mocks.
    ///
    /// Every expectation is unbounded and stateful, so a test asserts on the resulting storage
    /// rather than on how many times something was called.
    fn wire(fake: &Fake) -> (MockS3Impl, MockDynamoDb) {
        let mut s3 = MockS3Impl::default();
        let mut dynamodb = MockDynamoDb::default();

        let f = fake.clone();
        s3.expect_put_object()
            .returning(move |_, key, body, metadata| {
                let mut storage = f.lock();
                storage.objects.insert(
                    key.as_bytes().to_vec(),
                    (body.to_vec(), metadata.unwrap_or_default()),
                );

                if let Some((hash, state)) = storage.race_state.take() {
                    let item = serde_dynamo::to_item(FragmentStateEntry::new(hash, state)).unwrap();
                    storage.state.insert(hash.data().to_vec(), item);
                }

                Ok(PutObjectOutput::builder().build())
            });

        let f = fake.clone();
        s3.expect_get_object().returning(move |_, key, _| {
            let mut storage = f.lock();
            storage.object_reads += 1;
            match storage.objects.get(key.as_bytes()) {
                Some((body, metadata)) => Ok(GetObjectOutput::builder()
                    .set_body(Some(body.clone().into()))
                    .set_metadata(Some(metadata.clone()))
                    .build()),
                None => Err(aws_error(
                    GetObjectError::NoSuchKey(NoSuchKey::builder().build()),
                    404,
                )),
            }
        });

        let f = fake.clone();
        s3.expect_head_object().returning(move |_, key| {
            let mut storage = f.lock();
            storage.object_reads += 1;
            match storage.objects.get(key.as_bytes()) {
                Some((_, metadata)) => Ok(HeadObjectOutput::builder()
                    .set_metadata(Some(metadata.clone()))
                    .build()),
                None => Err(aws_error(
                    HeadObjectError::NotFound(NotFound::builder().build()),
                    404,
                )),
            }
        });

        let f = fake.clone();
        s3.expect_delete_object().returning(move |_, key, _| {
            if f.failing(Fault::ObjectDelete) {
                return Err(aws_error(
                    DeleteObjectError::generic(ErrorMetadata::builder().code("500").build()),
                    500,
                ));
            }

            f.lock().objects.remove(key.as_bytes());
            Ok(DeleteObjectOutput::builder().build())
        });

        let f = fake.clone();
        s3.expect_list_versions().returning(move |_, _| {
            if f.failing(Fault::ObjectList) {
                return Err(aws_error(
                    ListObjectVersionsError::generic(ErrorMetadata::builder().code("500").build()),
                    500,
                ));
            }

            Ok(ListObjectVersionsOutput::builder().build())
        });

        let f = fake.clone();
        dynamodb
            .expect_get_item()
            .returning(move |table, item, _| {
                if &**table == FRAGMENT_STATE_TABLE_NAME {
                    if f.failing(Fault::StateReadTimeout) {
                        return Err(AwsError::AwsSdkError(SdkError::timeout_error(Box::new(
                            std::io::Error::other("injected timeout"),
                        ))));
                    }
                    if f.failing(Fault::StateReadBroken) {
                        return Err(aws_error(
                            GetItemError::ResourceNotFoundException(
                                ResourceNotFoundException::builder().build(),
                            ),
                            400,
                        ));
                    }
                    if f.failing(Fault::StateRead) {
                        return Err(throughput_exceeded(
                            GetItemError::ProvisionedThroughputExceededException(
                                throttling_exception(),
                            ),
                        ));
                    }
                }

                let storage = f.lock();
                let found = if &**table == FRAGMENT_STATE_TABLE_NAME {
                    storage.state.get(&blob(&item, "hash")).cloned()
                } else if &**table == FRAGMENT_METADATA_TABLE_NAME {
                    storage.legacy_metadata.get(&blob(&item, "hash")).cloned()
                } else {
                    storage
                        .associations
                        .get(&(blob(&item, "hash"), blob(&item, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE)))
                        .cloned()
                };

                Ok(GetItemOutput::builder().set_item(found).build())
            });

        let f = fake.clone();
        dynamodb
            .expect_batch_get_item()
            .returning(move |table, keys, _| {
                let storage = f.lock();

                // Routed by table, like the single-item read above. Reading the state table - and
                // the legacy metadata table behind it - in one request each is what lets resolution
                // consult lifecycle state once per batch rather than once per address.
                Ok(keys
                    .iter()
                    .filter_map(|key| {
                        if &**table == FRAGMENT_STATE_TABLE_NAME {
                            storage.state.get(&blob(key, "hash")).cloned()
                        } else if &**table == FRAGMENT_METADATA_TABLE_NAME {
                            storage.legacy_metadata.get(&blob(key, "hash")).cloned()
                        } else {
                            storage
                                .associations
                                .get(&(
                                    blob(key, "hash"),
                                    blob(key, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
                                ))
                                .cloned()
                        }
                    })
                    .collect())
            });

        let f = fake.clone();
        dynamodb.expect_put_item().returning(move |table, item| {
            if &**table == FRAGMENTS_TABLE_NAME && f.failing(Fault::AssociationWrite) {
                return Err(throughput_exceeded(
                    PutItemError::ProvisionedThroughputExceededException(throttling_exception()),
                ));
            }

            let mut storage = f.lock();
            if &**table == FRAGMENT_STATE_TABLE_NAME {
                storage.state.insert(blob(&item, "hash"), item);
            } else {
                storage.associations.insert(
                    (
                        blob(&item, "hash"),
                        blob(&item, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
                    ),
                    item,
                );
            }
            Ok(PutItemOutput::builder().build())
        });

        let f = fake.clone();
        dynamodb
            .expect_put_item_conditional::<RowAbsent>()
            .returning(move |_, item, _| {
                let mut storage = f.lock();
                let key = blob(&item, "hash");

                if let Some(existing) = storage.state.get(&key) {
                    return Err(conditional_check_failed(existing.clone()));
                }

                storage.state.insert(key, item);
                Ok(PutItemOutput::builder().build())
            });

        let f = fake.clone();
        dynamodb
            .expect_put_item_conditional::<StateUnchanged>()
            .returning(move |_, item, condition| {
                if f.failing(Fault::StateWrite) {
                    return Err(throughput_exceeded(
                        PutItemError::ProvisionedThroughputExceededException(
                            throttling_exception(),
                        ),
                    ));
                }

                let mut storage = f.lock();
                let key = blob(&item, "hash");

                let current = storage.state.get(&key).map(|existing| {
                    serde_dynamo::from_item::<_, FragmentStateEntry>(existing.clone())
                        .unwrap()
                        .state()
                });

                if current == Some(condition.0) {
                    storage.state.insert(key, item);
                    Ok(PutItemOutput::builder().build())
                } else {
                    Err(conditional_check_failed(
                        storage.state.get(&key).cloned().unwrap_or_default(),
                    ))
                }
            });

        let f = fake.clone();
        dynamodb.expect_delete_item().returning(move |table, item| {
            let fault = if &**table == FRAGMENT_STATE_TABLE_NAME {
                Fault::StateDelete
            } else {
                Fault::AssociationDelete
            };
            if f.failing(fault) {
                return Err(throughput_exceeded(
                    DeleteItemError::ProvisionedThroughputExceededException(throttling_exception()),
                ));
            }

            let mut storage = f.lock();
            if &**table == FRAGMENT_STATE_TABLE_NAME {
                storage.state.remove(&blob(&item, "hash"));
            } else {
                storage.associations.remove(&(
                    blob(&item, "hash"),
                    blob(&item, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
                ));
                if let Some(deleted) = storage.association_deleted.take() {
                    let _ = deleted.send(());
                }
            }
            Ok(DeleteItemOutput::builder().build())
        });

        let f = fake.clone();
        dynamodb.expect_query_single().returning(move |_, query| {
            if f.failing(Fault::AssociationCount) {
                return Err(throughput_exceeded(
                    QueryError::ProvisionedThroughputExceededException(throttling_exception()),
                ));
            }

            let storage = f.lock();
            let FragmentsQuery::HashCount(hash) = query;
            let count = storage
                .associations
                .keys()
                .filter(|(stored, _)| stored == hash.data())
                .count();

            Ok(QueryOutput::builder()
                .count(i32::try_from(count).unwrap())
                .build())
        });

        (s3, dynamodb)
    }

    /// A throttling exception carrying the error code a real response would, which is what the
    /// classifier reads — a builder-constructed exception has no metadata at all.
    fn throttling_exception() -> ProvisionedThroughputExceededException {
        ProvisionedThroughputExceededException::builder()
            .meta(
                ErrorMetadata::builder()
                    .code("ProvisionedThroughputExceededException")
                    .build(),
            )
            .build()
    }

    fn throughput_exceeded<E>(error: E) -> AwsError<SdkError<E, HttpResponse>> {
        aws_error(error, 400)
    }

    fn conditional_check_failed(
        item: HashMap<String, AttributeValue>,
    ) -> AwsError<SdkError<PutItemError, HttpResponse>> {
        aws_error(
            PutItemError::ConditionalCheckFailedException(
                ConditionalCheckFailedException::builder()
                    .set_item(Some(item))
                    .build(),
            ),
            400,
        )
    }

    async fn store_with(
        fake: &Fake,
        force_write: bool,
        fragment_metadata: bool,
    ) -> Arc<AwsImmutableStore> {
        let (s3, dynamodb) = wire(fake);
        let mut dynamodb_settings = DynamoDbImmutableStoreSettings::new(
            FRAGMENTS_TABLE_NAME.to_string(),
            FRAGMENT_STATE_TABLE_NAME.to_string(),
        );
        dynamodb_settings.timeout_millis = 1;

        if fragment_metadata {
            dynamodb_settings = dynamodb_settings
                .with_fragment_metadata_table(FRAGMENT_STATE_TABLE_NAME.to_string());
        }

        let settings = AwsImmutableStoreSettings {
            s3: S3StoreSettings::new(BUCKET.to_string()),
            dynamodb: dynamodb_settings,
            force_write,
        };

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                Arc::new(AwsImmutableStore::new(s3, dynamodb, &settings))
            })
            .await
    }

    async fn store(fake: &Fake) -> Arc<AwsImmutableStore> {
        store_with(fake, false, false).await
    }

    /// A store on a deployment that may still hold objects written before fragments moved onto
    /// them, and so is configured to read the rows describing those.
    async fn migrated_store(fake: &Fake) -> Arc<AwsImmutableStore> {
        store_with(fake, false, true).await
    }

    /// A store whose state table and legacy-metadata table are two distinct ddb tables.
    ///
    /// `migrated_store` points both at `FRAGMENT_STATE_TABLE_NAME`, which means the same storage
    /// map backs both. That collapses the scenario where no state row exists but a metadata row
    /// does — `load_state` would find and interpret the metadata row as `Stored`. This helper uses
    /// `FRAGMENT_METADATA_TABLE_NAME` for the metadata table so the two maps are independent,
    /// enabling tests for the `do_query` path that falls back to the metadata table when there is
    /// genuinely no state row.
    async fn store_with_separate_metadata_table(fake: &Fake) -> Arc<AwsImmutableStore> {
        let (s3, dynamodb) = wire(fake);
        let mut dynamodb_settings = DynamoDbImmutableStoreSettings::new(
            FRAGMENTS_TABLE_NAME.to_string(),
            FRAGMENT_STATE_TABLE_NAME.to_string(),
        );
        dynamodb_settings.timeout_millis = 1;
        dynamodb_settings = dynamodb_settings
            .with_fragment_metadata_table(FRAGMENT_METADATA_TABLE_NAME.to_string());

        let settings = AwsImmutableStoreSettings {
            s3: S3StoreSettings::new(BUCKET.to_string()),
            dynamodb: dynamodb_settings,
            force_write: false,
        };

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                Arc::new(AwsImmutableStore::new(s3, dynamodb, &settings))
            })
            .await
    }

    /// A payload and the fragment that correctly describes it.
    fn representation(
        codec: FragmentFlags,
        size_payload: usize,
        size_content: u64,
    ) -> (Fragment, Bytes) {
        let payload = Bytes::from(vec![codec.bits() as u8; size_payload]);
        let fragment = Fragment {
            flags: codec.bits(),
            size_payload: u32::try_from(size_payload).unwrap(),
            size_content,
        };

        (fragment, payload)
    }

    #[tokio::test]
    async fn put_stores_the_fragment_on_the_object() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        store(&fake)
            .await
            .put(partition, address, fragment, Some(payload.clone()), false)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("first put should succeed");

        let (other, other_payload) = representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
        store
            .put(partition, address, other, Some(other_payload), false)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("put should succeed");

        let (loaded, bytes) = store
            .get(partition, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let result = query_one(&(store as Arc<dyn ImmutableStoreTrait>), partition, address)
            .await
            .expect("resolve should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert!(result.stored_durable);
        assert_eq!(
            fake.object_reads(),
            0,
            "resolve must not touch S3; it is called once per fragment on the ingress write path"
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let result = store
            .get_metadata(partition, address)
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
        let partition: Partition = random();
        let (fragment, _) = preexisting_object(&fake, partition, address);

        let result = migrated_store(&fake)
            .await
            .get_metadata(partition, address)
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

    /// The ordering the single-read existence path rests on: the association is written last, so
    /// nothing can observe one whose content is not already stored and published.
    #[tokio::test]
    async fn a_put_associates_only_after_publishing_the_state() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        assert!(fake.has_association(partition, address));
    }

    /// Obliteration drops the reference before it marks the hash, because the reference is the
    /// obligation and the mark is only bookkeeping for the reclamation that follows.
    #[tokio::test]
    async fn obliterate_deletes_the_association_before_marking_the_hash() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        store
            .clone()
            .obliterate(
                partition,
                address,
                Arc::new(StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate should succeed");

        assert!(!fake.has_association(partition, address));

        let result = query_one(&(store as Arc<dyn ImmutableStoreTrait>), partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        store(&fake)
            .await
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let (replacement, replacement_payload) =
            representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
        store_with(&fake, true, false)
            .await
            .put(
                partition,
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
    fn preexisting_object(
        fake: &Fake,
        partition: Partition,
        address: Address,
    ) -> (Fragment, Bytes) {
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.put_object_without_metadata(address.hash, payload.as_ref());
        fake.set_fragment_metadata_row(address.hash, fragment);
        fake.add_association(partition, address);

        (fragment, payload)
    }

    #[tokio::test]
    async fn get_falls_back_to_the_legacy_row_for_an_object_with_no_metadata() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (fragment, payload) = preexisting_object(&fake, partition, address);

        let (loaded, bytes) = migrated_store(&fake)
            .await
            .get(partition, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
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
        let partition: Partition = random();
        preexisting_object(&fake, partition, address);

        let result = query_one(
            &(migrated_store(&fake).await as Arc<dyn ImmutableStoreTrait>),
            partition,
            address,
        )
        .await
        .expect("resolve should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert_eq!(fake.object_reads(), 0, "resolve must not touch S3");
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
        let partition: Partition = random();
        preexisting_object(&fake, partition, address);

        store(&fake)
            .await
            .get(partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.put_object_with_damaged_metadata(address.hash, payload.as_ref());
        fake.set_fragment_metadata_row(address.hash, fragment);
        fake.add_association(partition, address);

        migrated_store(&fake)
            .await
            .get(partition, address)
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
            let partition: Partition = random();
            fake.add_association(partition, address);
            // No state row, no legacy metadata row.

            let result = query_one(
                &(store_with_separate_metadata_table(&fake).await as Arc<dyn ImmutableStoreTrait>),
                partition,
                address,
            )
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
            let partition: Partition = random();
            let obliterated = Fragment {
                flags: FragmentFlags::PayloadObliterated.bits(),
                size_payload: 64,
                size_content: 256,
            };

            fake.add_association(partition, address);
            fake.set_legacy_metadata_row(address.hash, obliterated);
            // No state row — the obliteration bit lives only in the legacy metadata flags.

            let result = query_one(
                &(store_with_separate_metadata_table(&fake).await as Arc<dyn ImmutableStoreTrait>),
                partition,
                address,
            )
            .await
            .expect("query should succeed");

            assert_eq!(result.match_made, StoreMatch::MatchNone);
        }

        /// What a push actually calls: one batch holding a legacy fragment, a current one and an
        /// address the store does not have. Each must resolve on its own terms, and the legacy rows
        /// must be read together rather than one round trip at a time.
        #[tokio::test]
        async fn query_resolves_a_batch_mixing_legacy_and_current_fragments() {
            let fake = Fake::default();
            let partition: Partition = random();
            let (fragment, _) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            let legacy = Address {
                hash: random(),
                context: random(),
            };
            fake.add_association(partition, legacy);
            fake.set_legacy_metadata_row(legacy.hash, fragment);

            let current = Address {
                hash: random(),
                context: random(),
            };
            fake.add_association(partition, current);
            fake.set_state(current.hash, FragmentState::Stored);

            let obliterated = Address {
                hash: random(),
                context: random(),
            };
            fake.add_association(partition, obliterated);
            fake.set_state(obliterated.hash, FragmentState::Obliterated);

            let absent = Address {
                hash: random(),
                context: random(),
            };

            let addresses = [legacy, current, obliterated, absent];
            let mut results = [StoreMatchResult::default(); 4];
            store_with_separate_metadata_table(&fake)
                .await
                .query(partition, &addresses, &mut results)
                .await
                .expect("query should succeed");

            assert_eq!(
                results.map(|result| result.match_made),
                [
                    StoreMatch::MatchFull,
                    StoreMatch::MatchFull,
                    StoreMatch::MatchNone,
                    StoreMatch::MatchNone
                ],
                "a legacy fragment and a current one both resolve, an obliterated one does not, \
                 and an address the store never had does not"
            );
        }

        /// A fragment stored before the state table existed: an association exists, no state row,
        /// but the metadata table holds the legacy fragment description. `query` must report a
        /// match — the legacy fallback exists precisely for this scenario.
        #[tokio::test]
        async fn query_matches_a_legacy_fragment_with_no_state_row() {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let partition: Partition = random();
            let (fragment, _) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            fake.add_association(partition, address);
            fake.set_legacy_metadata_row(address.hash, fragment);
            // No state row — `load_state` returns `Ok(None)`.

            let result = query_one(
                &(store_with_separate_metadata_table(&fake).await as Arc<dyn ImmutableStoreTrait>),
                partition,
                address,
            )
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
            let partition: Partition = random();
            let (mut fragment, payload) =
                representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
            fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

            fake.add_association(partition, address);
            fake.set_legacy_metadata_row(address.hash, fragment);
            fake.put_object_without_metadata(address.hash, payload.as_ref());
            // No state row — `do_query` returns `QueryResultSource::LegacyMetadata`.

            let result = store_with_separate_metadata_table(&fake)
                .await
                .get_metadata(partition, address)
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
            let partition: Partition = random();
            let (_, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            fake.add_association(partition, address);
            fake.set_state(address.hash, FragmentState::Stored);
            fake.put_object_without_metadata(address.hash, payload.as_ref());
            // No metadata row — `fragment_from_metadata_table` returns `Ok(None)`.

            store_with_separate_metadata_table(&fake)
                .await
                .get_metadata(partition, address)
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
            let partition: Partition = random();
            let (_, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

            fake.add_association(partition, address);
            fake.set_state(address.hash, FragmentState::Stored);
            fake.put_object_without_metadata(address.hash, payload.as_ref());
            // No legacy row — `fragment_from_metadata_table` returns `Ok(None)`, which
            // `get_s3_object_contents` maps to an error.

            store_with_separate_metadata_table(&fake)
                .await
                .get(partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);

        let error = store
            .clone()
            .get(partition, address)
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
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("re-put should succeed");

        assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));

        let (_, restored) = store
            .get(partition, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);

        let result = store
            .get_metadata(partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);
        fake.set_state(address.hash, FragmentState::Obliterating);

        store
            .get(partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.set_state(address.hash, FragmentState::Obliterated);

        store(&fake)
            .await
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("re-upload over a tombstone is allowed");

        assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        assert_eq!(fake.stored_fragment(address.hash), Some(fragment));
        assert_eq!(fake.association_count(address.hash), 1);
    }

    /// Store a fragmented parent whose payload is a list of references to `leaves`, all under one
    /// partition and context so obliteration walks from the parent into each leaf.
    async fn store_fragmented(
        store: &Arc<AwsImmutableStore>,
        partition: Partition,
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
                    partition,
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
            .put(partition, parent, fragment, Some(payload), false)
            .await
            .expect("parent put should succeed");

        parent
    }

    #[tokio::test]
    async fn obliterate_recurses_into_sub_fragments() {
        let fake = Fake::default();
        let partition: Partition = random();
        let context: Context = random();
        let leaves = [random::<Hash>(), random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, partition, context, &leaves).await;

        let stats = Arc::new(StoreObliterateStats::default());
        store
            .obliterate(partition, parent, stats.clone())
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
        let partition: Partition = random();
        let context: Context = random();
        let shared = random::<Hash>();
        let leaves = [shared, random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, partition, context, &leaves).await;

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
            .obliterate(partition, parent, Arc::new(StoreObliterateStats::default()))
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
        let partition: Partition = random();
        let context: Context = random();
        let leaves = [random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, partition, context, &leaves).await;

        store
            .obliterate(partition, parent, Arc::new(StoreObliterateStats::default()))
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let stats = Arc::new(StoreObliterateStats::default());
        store
            .obliterate(partition, address, stats.clone())
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
        let partition: Partition = random();
        let other_partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, mine, fragment, Some(payload.clone()), false)
            .await
            .expect("put should succeed");
        store
            .clone()
            .put(other_partition, theirs, fragment, Some(payload), false)
            .await
            .expect("second put should succeed");

        store
            .obliterate(partition, mine, Arc::new(StoreObliterateStats::default()))
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
        let partition: Partition = random();
        preexisting_object(&fake, partition, address);

        let store = migrated_store(&fake).await;
        fake.fail(Fault::StateRead);

        let error = store
            .get(partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.fail(Fault::AssociationWrite);

        store(&fake)
            .await
            .put(partition, address, fragment, Some(payload.clone()), false)
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
            .put(partition, address, fragment, Some(payload), false)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.fail(Fault::AssociationCount);

        store
            .obliterate(
                partition,
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.fail(Fault::ObjectDelete);

        store
            .obliterate(
                partition,
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.fail(Fault::ObjectList);

        store
            .obliterate(
                partition,
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
        let partition: Partition = random();
        let context: Context = random();
        let leaves = [random::<Hash>(), random::<Hash>()];

        let store = store(&fake).await;
        let parent = store_fragmented(&store, partition, context, &leaves).await;

        fake.fail(Fault::ObjectDelete);

        store
            .obliterate(partition, parent, Arc::new(StoreObliterateStats::default()))
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        fake.lose_object(address.hash);
        fake.fail(Fault::StateDelete);

        let error = store
            .get(partition, address)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.obliterate_during_upload(address.hash, FragmentState::Obliterating);

        let error = store(&fake)
            .await
            .put(partition, address, fragment, Some(payload), false)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.obliterate_during_upload(address.hash, FragmentState::Obliterated);

        store(&fake)
            .await
            .put(partition, address, fragment, Some(payload), false)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, mine, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let deleted = fake.association_deleted();
        let injector = fake.clone();
        let racing_partition: Partition = random();
        let racing = Address {
            hash,
            context: random(),
        };
        let mut tasks = JoinSet::new();
        lore_base::lore_spawn!(tasks, async move {
            deleted
                .await
                .expect("the obliteration must delete its association");
            injector.add_association(racing_partition, racing);
        });

        store
            .obliterate(partition, mine, Arc::new(StoreObliterateStats::default()))
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, source, fragment, Some(payload.clone()), false)
            .await
            .expect("put should succeed");

        let destination_context: Context = random();
        store
            .copy(partition, source, partition, destination_context, true)
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
        let partition: Partition = random();

        store(&fake)
            .await
            .copy(partition, source, partition, random::<Context>(), true)
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
        let partition: Partition = random();
        let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let store = store(&fake).await;
        store
            .clone()
            .put(partition, stored, fragment, Some(payload), false)
            .await
            .expect("put should succeed");

        let other_context = Address {
            hash,
            context: random(),
        };
        store
            .copy(
                partition,
                other_context,
                partition,
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
        let partition: Partition = random();
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
                .put(partition, address, fragment, Some(payload.clone()), false)
                .await
                .expect("put should succeed");
            stored.push(address);
        }

        let addresses = [stored[0], absent, stored[1]];
        let mut results = [StoreMatchResult::default(); 3];
        store
            .query(partition, &addresses, &mut results)
            .await
            .expect("resolve should succeed");

        assert_eq!(
            results.map(|result| result.match_made),
            [
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
                let partition: Partition = random();

                lore_base::lore_spawn!(writers, async move {
                    store
                        .put(partition, address, fragment, Some(payload), false)
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

    /// The store contract, checked against this store the same way it is checked against every
    /// other one.
    ///
    /// Note what this does *not* reach. The store is built without the legacy metadata table, so
    /// nothing here exercises the fallback resolution that table turns on, nor the gap that comes
    /// with it: a hash with no state row is left associated by `obliterate` and goes on matching
    /// through the fallback. Those paths are covered separately, on
    /// [`store_with_separate_metadata_table`], where the two tables are genuinely distinct.
    #[tokio::test]
    async fn satisfies_the_immutable_store_contract() {
        let fake = Fake::default();
        let store = store(&fake).await;

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                lore_storage::conformance::verify_immutable_store(
                    store,
                    lore_storage::conformance::Capabilities::new("AwsImmutableStore"),
                )
                .await;
            })
            .await;
    }
}
