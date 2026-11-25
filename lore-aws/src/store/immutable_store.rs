// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Once;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
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
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore as ImmutableStoreTrait;
use lore_storage::Oversized;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::PARTITION_ID;
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

use crate::aws::error::AwsCallError;
use crate::aws::error::ErrorPayload;
use crate::aws::error::S3ErrorCode;
use crate::aws::item::AttributeValue;
use crate::aws::item::from_item;
use crate::aws::item::to_item;
use crate::aws::s3::HeadObjectOutput;
use crate::aws_error::AwsError;
use crate::default_aws_timeout_millis;
use crate::dynamodb::ConditionParts;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbCondition;
use crate::dynamodb::DynamoDbQuery;
use crate::dynamodb::ExprNames;
use crate::dynamodb::ExprValues;
use crate::s3::S3;
use crate::s3::S3Error;
use crate::store::object_metadata::ObjectMetadataError;
use crate::store::object_metadata::from_object_metadata;
use crate::store::object_metadata::to_object_metadata;
use crate::store::single_flight::Role;
use crate::store::single_flight::SingleFlight;
use crate::store::store_failure;
use crate::store::with_backoff;

pub mod metadata_migrator;

enum QueryResultSource {
    LegacyMetadata(Fragment),
    State,
}

#[lore_macro::test_pub]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FragmentsEntry {
    pub(crate) hash: Hash,
    /// The partition that holds the association, followed by the address's context. Stored under
    /// its original attribute name, which predates the split between the two.
    #[serde(with = "serde_bytes", rename = "repository_context")]
    pub(crate) partition_context: [u8; size_of::<Context>() * 2],
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
    #[lore_macro::test_pub]
    pub(crate) fn new(partition: Partition, address: Address) -> Self {
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FragmentStateEntry {
    hash: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<u32>,
}

/// A row in the shape written before fragments moved onto the S3 object: the whole fragment,
/// flattened alongside the hash.
///
/// Deserialization only: nothing writes this shape.
#[lore_macro::test_pub]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FragmentMetadataEntry {
    pub(crate) hash: Hash,
    #[serde(flatten)]
    pub(crate) fragment: Option<Fragment>,
}

impl FragmentStateEntry {
    /// The key-only shape, with no state attribute.
    #[lore_macro::test_pub]
    fn key(hash: Hash) -> Self {
        Self { hash, state: None }
    }

    #[lore_macro::test_pub]
    pub(crate) fn new(hash: Hash, state: FragmentState) -> Self {
        Self {
            hash,
            state: Some(state.bits()),
        }
    }

    #[lore_macro::test_pub]
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
    /// `fragment_state_table_name`, since both row shapes share it and are told apart by shape.
    /// Leaving it unset declares that no such object exists, which makes an object carrying no
    /// metadata what it then is: damaged, rather than merely old.
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

/// Whether a hash still holds any association, in any partition.
///
/// The only `Query` this store issues. Everything else it asks is a keyed read: it reads no wider
/// than the exact association, so there is nothing to scan a hash partition for except deciding
/// whether the last reference to a payload has gone.
///
/// Existence rather than a count, and one row is enough to settle it. A count would have to be
/// paginated to be true — `query_single` issues one `Query` and never follows `last_evaluated_key`,
/// so a `Select::Count` here would report only what one page scanned and stop silently at
/// `DynamoDB`'s 1 MB limit.
#[lore_macro::test_pub]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FragmentsQuery(pub(crate) Hash);

impl DynamoDbQuery for FragmentsQuery {
    fn key_condition_expression(&self) -> &str {
        "#pk = :hash"
    }

    fn expression_attribute_names(&self) -> ExprNames {
        ExprNames::from_slice(&[("#pk", FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)])
    }

    fn expression_attribute_values(&self) -> ExprValues {
        smallvec::smallvec![(":hash", AttributeValue::blob(self.0.data()))]
    }

    /// One row answers the question, so the read stops there rather than scanning a hash partition
    /// that deduplication can have made arbitrarily wide.
    fn limit(&self) -> Option<i32> {
        Some(1)
    }

    /// Obliteration deletes an association and then asks this before destroying the payload, so an
    /// eventually-consistent empty answer would take content another partition still references.
    fn consistent_read(&self) -> bool {
        true
    }
}

/// Whether a partition holds any association for a hash, whatever context it is under.
///
/// The sort key is the partition followed by the context, so one prefix bounds every association a
/// partition holds for a hash and the read never leaves that partition — an unassociated hash and
/// one associated only elsewhere answer the same way, which is what isolation requires.
///
/// A copy asks this only when its source names no context, and pays a `Query` for it where an exact
/// source costs a keyed read. One row settles it, for the reason on [`FragmentsQuery`].
#[lore_macro::test_pub]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartitionAssociationQuery(pub(crate) Hash, pub(crate) Partition);

impl DynamoDbQuery for PartitionAssociationQuery {
    fn key_condition_expression(&self) -> &str {
        "#pk = :hash AND begins_with(#sk, :partition)"
    }

    fn expression_attribute_names(&self) -> ExprNames {
        ExprNames::from_slice(&[
            ("#pk", FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE),
            ("#sk", FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
        ])
    }

    fn expression_attribute_values(&self) -> ExprValues {
        smallvec::smallvec![
            (":hash", AttributeValue::blob(self.0.data())),
            (":partition", AttributeValue::blob(self.1.data())),
        ]
    }

    fn limit(&self) -> Option<i32> {
        Some(1)
    }

    /// A copy acts on the answer by writing an association, so it must not be satisfied by a view
    /// that predates an obliteration deleting the last one.
    fn consistent_read(&self) -> bool {
        true
    }
}

/// Write only if no row exists for this hash yet.
///
/// Publishing a payload uses this so that a concurrent obliteration's mark cannot be erased by a
/// racing writer: the writer's create loses, it re-reads the row, and it sees the mark.
#[lore_macro::test_pub]
#[derive(Debug, PartialEq)]
pub(crate) struct RowAbsent;

impl DynamoDbCondition for RowAbsent {
    fn into_parts(self) -> ConditionParts {
        ConditionParts {
            condition_expression: "attribute_not_exists(#hash)",
            expression_names: ExprNames::from_slice(&[("#hash", "hash")]),
            expression_values: ExprValues::new(),
        }
    }
}

/// Write only if the row is still in the state the caller last observed.
///
/// Obliteration advances the row through its states with this, so two obliterations racing for the
/// same hash cannot both believe they hold the mark.
#[lore_macro::test_pub]
#[derive(Debug, PartialEq)]
pub(crate) struct StateUnchanged(pub(crate) FragmentState);

impl DynamoDbCondition for StateUnchanged {
    fn into_parts(self) -> ConditionParts {
        ConditionParts {
            condition_expression: "#state = :state",
            expression_names: ExprNames::from_slice(&[("#state", "state")]),
            expression_values: smallvec::smallvec![(
                ":state",
                AttributeValue::number(self.0.bits())
            )],
        }
    }
}

/// Counts reads that found a partition still referencing a hash whose payload S3 no longer has.
///
/// Non-zero means content has been lost. The read itself is reported as a plain not-found, which is
/// indistinguishable from content that was never stored, so without this the loss is silent.
///
/// Bare, like every other metric here: [`InstrumentProvider::scope_name`] prefixes the provider's
/// namespace, which already reads `urc.store.immutable.aws`.
const METRICS_MISSING_PAYLOAD_METRIC_NAME: &str = "missing_payload";

/// Counts reads that found an association whose hash has no lifecycle state recorded anywhere.
///
/// A put publishes the state before it writes the association, so a correctly written fragment
/// always has one. Content stored before the state table existed does not, and is read from the
/// legacy metadata table instead — which is only consulted when that table is configured. So a
/// non-zero count means either a deployment holding legacy content without
/// `fragment_metadata_table_name` set, or a put that stopped between publishing and associating.
/// The read reports absence either way, so without this the cause is invisible.
///
/// Bare, for the reason on [`METRICS_MISSING_PAYLOAD_METRIC_NAME`].
const METRICS_ASSOCIATION_WITHOUT_STATE_METRIC_NAME: &str = "association_without_state";

/// Counts shared association and state reads by `role`. A `leader` issues the `DynamoDB` read; a
/// `follower` uses the result of a read already in progress for the same key. `read` names the
/// read, and `context` names the operation that requested it.
///
/// Bare, for the reason on [`METRICS_MISSING_PAYLOAD_METRIC_NAME`].
const METRICS_SHARED_READS_METRIC_NAME: &str = "shared_reads";

/// Lower bound on the obliteration drain, regardless of how the `DynamoDB` timeout is configured.
const MIN_OBLITERATION_DRAIN_MILLIS: u64 = 100;

/// Mark a fragment as durably stored.
///
/// Durability is a fact about this store, not about the payload, so it is derived on read rather
/// than written down: an object present in the bucket is durable by definition. Persisting it would
/// mean serving one store's answer for another, and would let the claim outlive the object.
fn stored_durable(mut fragment: Fragment) -> Fragment {
    fragment.flags |= FragmentFlags::PayloadStoredDurable.bits();
    fragment
}

/// Refuse an object larger than any fragment this store can legitimately hold.
///
/// Reported from both the declared length and the bytes actually read, because a chunked reply
/// declares no length and only the second bound covers it.
fn oversized_object(hash: Hash, size: usize, max: usize) -> StoreError {
    warn!(
        %hash,
        size,
        max,
        "S3 object exceeds maximum allowed size; treating as malicious"
    );

    StoreError::from(Oversized {
        context: format!("S3 object size {size} for {hash} exceeds maximum {max}"),
    })
}

/// Whether S3 answered that the requested object does not exist, with a code `absent` accepts.
fn names_no_object(error: &S3Error, absent: fn(&S3ErrorCode) -> bool) -> bool {
    matches!(error, AwsError::Aws(AwsCallError::Service(service)) if absent(&service.code))
}

static STORE_ATTRIBUTES: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("store", "aws")]);

/// The operation a shared read was made for, which the `shared_reads` counter names in `context`.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
enum SharedReadContext {
    Get,
    Put,
    Copy,
    GetMetadata,
}

impl SharedReadContext {
    const ALL: [Self; 4] = [Self::Get, Self::Put, Self::Copy, Self::GetMetadata];

    fn name(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Put => "put",
            Self::Copy => "copy",
            Self::GetMetadata => "get_metadata",
        }
    }

    fn reads(self) -> &'static SharedReads {
        &SHARED_READS[self as usize]
    }
}

/// One operation's shared reads: association reads, which the counter names `exists` in `read`,
/// and state reads, which it names `state`.
struct SharedReads {
    association: RoleCounts,
    state: RoleCounts,
}

impl SharedReads {
    const fn new() -> Self {
        Self {
            association: RoleCounts::new(),
            state: RoleCounts::new(),
        }
    }
}

/// Reads by the [`Role`] their caller played.
struct RoleCounts {
    leader: AtomicU64,
    follower: AtomicU64,
}

impl RoleCounts {
    const fn new() -> Self {
        Self {
            leader: AtomicU64::new(0),
            follower: AtomicU64::new(0),
        }
    }

    fn add(&self, role: Role) {
        let count = match role {
            Role::Leader => &self.leader,
            Role::Follower => &self.follower,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Every store's shared reads, indexed by [`SharedReadContext`]. Counting a read is a relaxed
/// increment that neither hashes labels nor takes the meter's lock, as adding to a counter
/// instrument does; the `shared_reads` counter reads these when the meter collects. They are
/// process-wide because the counter's series are: stores record into the same series, and an
/// observation replaces a series' value rather than adding to it.
static SHARED_READS: [SharedReads; 4] = [const { SharedReads::new() }; 4];

/// Registers the `shared_reads` counter to report [`SHARED_READS`], once per process. A series is
/// reported from its first read on, as a counter instrument's is.
fn report_shared_reads(provider: &AwsImmutableStoreInstrumentProvider) {
    static REPORTED: Once = Once::new();
    REPORTED.call_once(|| {
        let mut series: Vec<(&'static AtomicU64, LabelArray)> = Vec::with_capacity(16);
        for context in SharedReadContext::ALL {
            let reads = context.reads();
            for (read, counts) in [("exists", &reads.association), ("state", &reads.state)] {
                for (role, count) in [("leader", &counts.leader), ("follower", &counts.follower)] {
                    let mut labels = provider.get_labels_for_operation_context(context.name());
                    labels.push(KeyValue::new("read", read));
                    labels.push(KeyValue::new("role", role));
                    series.push((count, labels));
                }
            }
        }
        provider
            .meter()
            .u64_observable_counter(provider.scope_name(METRICS_SHARED_READS_METRIC_NAME))
            .with_callback(move |observer| {
                for (count, labels) in &series {
                    let count = count.load(Ordering::Relaxed);
                    if count > 0 {
                        observer.observe(count, labels);
                    }
                }
            })
            .build();
    });
}

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
    /// Table of [`FragmentStateEntry`] rows: lifecycle state only, never a fragment.
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
    labels_query: LabelArray,
    /// Both of these are emitted with the calling operation's own label array rather than one of
    /// their own, so `context` names the operation that observed the condition. The metric name
    /// already says which condition it is, and either can be observed from more than one operation.
    missing_payload_counter: Counter<u64>,
    association_without_state_counter: Counter<u64>,
    association_reads: SingleFlight<(Partition, Address), Result<bool, StoreError>>,
    state_reads: SingleFlight<Hash, Result<Option<FragmentState>, StoreError>>,
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
        let labels_query = provider.get_labels_for_operation_context("query");
        let missing_payload_counter = provider.counter(METRICS_MISSING_PAYLOAD_METRIC_NAME);
        let association_without_state_counter =
            provider.counter(METRICS_ASSOCIATION_WITHOUT_STATE_METRIC_NAME);
        report_shared_reads(&provider);
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
            labels_query,
            missing_payload_counter,
            association_without_state_counter,
            association_reads: SingleFlight::default(),
            state_reads: SingleFlight::default(),
        }
    }

    /// Checks whether this partition holds the association for this address. Concurrent callers
    /// checking the same address share the read, which is counted for `context`.
    #[lore_macro::test_pub]
    async fn exists(
        &self,
        partition: Partition,
        address: Address,
        context: SharedReadContext,
    ) -> Result<bool, StoreError> {
        let (exists, role) = self
            .association_reads
            .run((partition, address), || {
                self.read_association(partition, address)
            })
            .await;
        context.reads().association.add(role);
        exists
    }

    /// Reads the lifecycle state of a hash, sharing the read with concurrent callers querying the
    /// same hash. The shared read can begin before the caller's request. It always begins after
    /// any state write by this store that completed before the request began. A caller must
    /// tolerate a read racing an obliteration's mark. A step that needs a read issued after its
    /// own request began, such as taking or releasing that mark, must read unshared. The read is
    /// counted for `context`.
    #[lore_macro::test_pub]
    async fn load_state_shared(
        &self,
        hash: Hash,
        context: SharedReadContext,
    ) -> Result<Option<FragmentState>, StoreError> {
        let (state, role) = self.state_reads.run(hash, || self.load_state(hash)).await;
        context.reads().state.add(role);
        state
    }

    /// Whether this partition holds the association for this address, which is the whole of what
    /// this store will serve: it isolates partitions, so it reads no wider than the association
    /// asked about.
    async fn read_association(
        &self,
        partition: Partition,
        address: Address,
    ) -> Result<bool, StoreError> {
        let entry = FragmentsEntry::new(partition, address);
        let item = to_item(&entry).map_err(|e| {
            warn!(
                "Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e:?}",
            );
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment entry for DynamoDB lookup",
            )
        })?;

        self.dynamodb
            .item_exists(
                &self.fragments_table_name,
                item,
                true, /* consistent read */
            )
            .await
            .map_err(|e| store_failure(e, "DynamoDB fragment lookup failed"))
    }

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
            items.push(to_item(&entry).map_err(|e| {
                warn!("Failed to serialize fragment entry {entry:?} for resolve: {e:?}");
                StoreError::internal_with_context(e, "Failed to serialize fragment entry")
            })?);
        }

        let output = self
            .dynamodb
            .batch_get_item(&self.fragments_table_name, items, true)
            .await
            .map_err(|e| store_failure(e, "DynamoDB fragment association batch read failed"))?;

        let mut present = HashSet::with_capacity(output.len());
        for item in output {
            match from_item::<FragmentsEntry>(item) {
                Ok(entry) => {
                    present.insert((&entry).into());
                }
                Err(e) => warn!("Failed to convert dynamo item to fragments entry: {e:?}"),
            }
        }

        Ok(present)
    }

    /// Resolve whether this partition may read an address, from `DynamoDB` alone.
    ///
    /// A full match or nothing: this store reads no wider than the exact association, so there is
    /// no weaker level for a miss to fall back to. A match needs the association and a `Stored`
    /// state; a hash under obliteration, or obliterated, is a miss. A match on the state table
    /// carries `Fragment::default()` marked durable, since the representation lives on the object.
    /// A hash with no state row resolves from the legacy metadata table when one is configured, and
    /// a match there carries the fragment its row holds.
    ///
    /// `labels` is the calling operation's label array for counting associations without state.
    /// Its `context` names the operation rather than the condition. The caller supplies these
    /// labels because the condition doesn't identify which read noticed it. The shared reads it
    /// makes are counted for `context`, the same operation.
    async fn do_query(
        &self,
        partition: Partition,
        address: Address,
        labels: &[KeyValue],
        context: SharedReadContext,
    ) -> Result<(QueryResultSource, StoreGetData), StoreError> {
        let (associated, state) = tokio::join!(
            self.exists(partition, address, context),
            self.load_state_shared(address.hash, context)
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

                self.association_without_state_counter.add(1, labels);
                trace!("Query found an association at {address} with no stored payload");
                miss
            }
        }
    }

    /// The batch resolution behind [`ImmutableStore::query`], kept out of the trait method so the
    /// latency record survives the `?` on the reads below.
    async fn do_query_batch(
        &self,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
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

                // Counted here as well as in `do_query`, because this is the path that resolves in
                // bulk: leaving it to the single-address reads would let a store lose content and
                // still report zero for as long as nothing asked for a representation.
                if state.is_none() {
                    self.association_without_state_counter
                        .add(1, &self.labels_query);
                    trace!("Query found an association at {address} with no stored payload");
                }

                if !matches!(state, Some(FragmentState::Stored)) {
                    *result = StoreMatchResult::default();
                    continue;
                }
            }

            *result = StoreMatchResult {
                match_made: StoreMatch::MatchFull,
                // This store isolates, so it only ever matches inside the partition asked about.
                partition,
                context: address.context,
                stored_local: false,
                stored_durable: true,
            };
        }

        Ok(())
    }

    /// Record that a payload exists, without disturbing an obliteration that may hold the hash.
    ///
    /// The create is conditional, so it can never overwrite a mark. Losing that condition is the
    /// ordinary outcome for content that is already stored — the row carries no representation, so
    /// there is nothing to reconcile and the existing row is already correct. The state it carries
    /// is returned so the caller can tell "already published" from "an obliteration holds this".
    #[lore_macro::test_pub]
    async fn publish_state(&self, hash: Hash) -> Result<FragmentState, StoreError> {
        let entry = FragmentStateEntry::new(hash, FragmentState::Stored);
        let item = to_item(&entry).map_err(|e| {
            warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize fragment state for DynamoDB")
        })?;

        match self
            .state_reads
            .write(hash, || {
                self.dynamodb
                    .put_item_conditional(&self.fragment_state_table_name, item, RowAbsent)
            })
            .await
        {
            Ok(_) => Ok(FragmentState::Stored),
            Err(AwsError::Aws(AwsCallError::Service(service)))
                if service.code.is_conditional_check_failed_exception() =>
            {
                let ErrorPayload::Item(item) = service.payload else {
                    return Ok(FragmentState::Stored);
                };

                Ok(from_item::<FragmentStateEntry>(*item)
                    .inspect_err(|e| {
                        warn!(%hash, "Failed to parse the fragment state a create lost against: {e}");
                    })
                    .map_or(FragmentState::Stored, |entry| entry.state()))
            }
            Err(e) => Err(store_failure(e, "DynamoDB fragment state write failed")),
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
    ///
    /// `labels` carries the calling operation's context, for the reason on [`Self::do_query`].
    async fn report_missing_payload(&self, address: Address, labels: &[KeyValue]) {
        self.missing_payload_counter.add(1, labels);
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
    #[lore_macro::test_pub]
    async fn clear_state(&self, hash: Hash) -> Result<(), StoreError> {
        let item = to_item(FragmentStateEntry::key(hash)).map_err(|e| {
            warn!("Failed to serialize fragment state key for {hash}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize fragment state for delete")
        })?;

        self.state_reads
            .write(hash, || {
                self.dynamodb
                    .delete_item(&self.fragment_state_table_name, item)
            })
            .await
            .map_err(|e| store_failure(e, "DynamoDB fragment state delete failed"))?;

        Ok(())
    }

    /// Move a tombstoned hash back to stored, now that its payload has been uploaded again.
    ///
    /// Losing this race is not a failure. Another writer reviving the same tombstone produced
    /// exactly the state this one wanted, and this one's bytes are already uploaded, so there is
    /// nothing left to disagree about. Only finding the hash back under an obliteration is a reason
    /// to stop, and that is a back-off rather than an error because the mark is transient. A write
    /// that fails, rather than losing the race, is reported as it failed.
    ///
    /// [`AwsImmutableStore::take_mark`] has no such tolerance: treating "already in the target
    /// state" as success there would let two obliterations both believe they hold the mark.
    #[lore_macro::test_pub]
    async fn revive_state(&self, hash: Hash) -> Result<(), StoreError> {
        if self
            .compare_and_set_state(hash, FragmentState::Obliterated, FragmentState::Stored)
            .await?
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

    /// Take the obliteration mark on `hash`, failing if its row is no longer `Stored`.
    ///
    /// Because the row holds nothing but the state, this compare-and-set is over a single attribute
    /// and two obliterations racing for the mark cannot both win. A lost race is terminal: the row
    /// says the work is not ours to do, and asking again will not change that.
    #[lore_macro::test_pub]
    async fn take_mark(&self, hash: Hash) -> Result<(), StoreError> {
        if self
            .compare_and_set_state(hash, FragmentState::Stored, FragmentState::Obliterating)
            .await?
        {
            return Ok(());
        }

        warn!("Fragment state for {hash} was not Stored when taking the obliteration mark");
        Err(StoreError::internal(
            "Failed to update fragment state due to conflict",
        ))
    }

    /// Move a marked `hash` into `target`: `Stored` releases the mark, `Obliterated` finalizes it.
    ///
    /// Only the mark's holder moves a row out of `Obliterating`, so a row already in `target` is
    /// this obliteration's own earlier attempt, applied although its answer was lost, and counts as
    /// done. A row in any other state is a conflict.
    #[lore_macro::test_pub]
    async fn clear_mark(&self, hash: Hash, target: FragmentState) -> Result<(), StoreError> {
        if self
            .compare_and_set_state(hash, FragmentState::Obliterating, target)
            .await?
        {
            return Ok(());
        }

        match self.load_state(hash).await? {
            Some(state) if state == target => {
                debug!(%hash, ?target, "An earlier attempt already cleared the obliteration mark");
                Ok(())
            }
            state => {
                warn!(%hash, ?state, ?target, "Fragment state moved under a held obliteration mark");
                Err(StoreError::internal(
                    "Failed to update fragment state due to conflict",
                ))
            }
        }
    }

    /// Run one step of an obliteration, from taking its mark onwards, retrying it on the store
    /// ladder while it is shed.
    ///
    /// Every such step is safe to repeat. One still shed when the ladder runs out is reported as an
    /// internal error rather than a back-off: a caller retrying the obliteration would find the mark
    /// held and return early, reporting work left undone as complete.
    async fn marked_step<T, F, Fut>(step: F) -> Result<T, StoreError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
    {
        with_backoff(StoreError::is_slow_down, step)
            .await
            .map_err(|error| {
                if error.is_slow_down() {
                    StoreError::internal_with_context(
                        error,
                        "Obliteration step still shed after the store ladder",
                    )
                } else {
                    error
                }
            })
    }

    /// Move the state row from `expected` to `updated`, answering `false` when the row was not in
    /// `expected` and so was left alone.
    async fn compare_and_set_state(
        &self,
        hash: Hash,
        expected: FragmentState,
        updated: FragmentState,
    ) -> Result<bool, StoreError> {
        let entry = FragmentStateEntry::new(hash, updated);
        let item = to_item(&entry).map_err(|e| {
            warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
            StoreError::internal_with_context(e, "Failed to serialize fragment state for DynamoDB")
        })?;

        match self
            .state_reads
            .write(hash, || {
                self.dynamodb.put_item_conditional(
                    &self.fragment_state_table_name,
                    item,
                    StateUnchanged(expected),
                )
            })
            .await
        {
            Ok(_) => Ok(true),
            Err(AwsError::Aws(AwsCallError::Service(ref service)))
                if service.code.is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(store_failure(
                e,
                "DynamoDB conditional fragment state update failed",
            )),
        }
    }

    async fn associate_fragment(
        &self,
        partition: Partition,
        address: Address,
    ) -> Result<(), StoreError> {
        let entry = FragmentsEntry::new(partition, address);

        let item = to_item(&entry).map_err(|e| {
            warn!("Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment association for DynamoDB",
            )
        })?;

        self.association_reads
            .write((partition, address), || {
                self.dynamodb.put_item(&self.fragments_table_name, item)
            })
            .await
            .map_err(|e| store_failure(e, "DynamoDB fragment association write failed"))?;

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
            items.push(to_item(FragmentStateEntry::key(*hash)).map_err(|e| {
                warn!("Failed to serialize fragment state entry for {hash}: {e:?}");
                StoreError::internal_with_context(e, "Failed to serialize fragment state entry")
            })?);
        }

        let output = self
            .dynamodb
            .batch_get_item(&self.fragment_state_table_name, items, true)
            .await
            .map_err(|e| store_failure(e, "DynamoDB fragment state batch read failed"))?;

        let mut states = HashMap::with_capacity(output.len());
        for item in output {
            match from_item::<FragmentStateEntry>(item) {
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
    /// in the flags of its metadata row instead. Without this an address stored in that era
    /// resolves to absence, and a push would ask a client to upload content it does not have and
    /// the store already holds.
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
            items.push(to_item(FragmentStateEntry::key(*hash)).map_err(|e| {
                warn!("Failed to serialize legacy fragment key for {hash}: {e:?}");
                StoreError::internal_with_context(
                    e,
                    "Failed to serialize fragment entry for legacy metadata load",
                )
            })?);
        }

        let output = self
            .dynamodb
            .batch_get_item(table_name, items, true /* consistent read */)
            .await
            .map_err(|e| store_failure(e, "DynamoDB legacy fragment metadata batch read failed"))?;

        let mut states = HashMap::with_capacity(output.len());
        for item in output {
            match from_item::<FragmentMetadataEntry>(item) {
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

    /// Whether `partition` holds any association for `hash`, whatever context it is under.
    async fn has_partition_association(
        &self,
        partition: Partition,
        hash: Hash,
    ) -> Result<bool, StoreError> {
        self.dynamodb
            .query_single(
                &self.fragments_table_name,
                PartitionAssociationQuery(hash, partition),
            )
            .await
            .map(|output| output.count > 0)
            .map_err(|e| store_failure(e, "DynamoDB partition association query failed"))
    }

    async fn has_associations(&self, hash: Hash) -> Result<bool, StoreError> {
        self.dynamodb
            .query_single(&self.fragments_table_name, FragmentsQuery(hash))
            .await
            .map(|output| output.count > 0)
            .map_err(|e| store_failure(e, "DynamoDB fragment association query failed"))
    }

    #[lore_macro::test_pub]
    async fn delete_association(
        &self,
        partition: Partition,
        address: Address,
    ) -> Result<(), StoreError> {
        let entry = FragmentsEntry::new(partition, address);

        let item = to_item(&entry).map_err(|e| {
            warn!("Failed to convert fragment entry: {entry:?} to dynamo attribute value map: {e}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment association for DynamoDB delete",
            )
        })?;

        self.association_reads
            .write((partition, address), || {
                self.dynamodb.delete_item(&self.fragments_table_name, item)
            })
            .await
            .map_err(|e| store_failure(e, "DynamoDB fragment association delete failed"))?;

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
                    payload,
                    Some(to_object_metadata(&fragment)),
                )
                .await
                .map(|_| ())
                .map_err(|e| store_failure(e, "S3 put object failed"))?;
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

    /// Permanently delete a payload from S3 by removing every stored version of its object.
    ///
    /// Delete markers are left in place. When the listing names no stored version, the unversioned
    /// delete is sent instead.
    async fn delete_payload(&self, hash: Hash) -> Result<(), StoreError> {
        let mut dst = [0u8; 64];
        let key = lore_revision::util::to_hex_str(hash.data(), &mut dst);

        let listing = self
            .s3
            .list_versions(self.bucket.as_str(), key)
            .await
            .map_err(|e| store_failure(e, "S3 list object versions failed"))?;

        // A partial listing must not be reported as a completed obliteration.
        if listing.truncated {
            warn!(
                %hash,
                versions = listing.versions.len(),
                "S3 could not list every version of a hash being obliterated; retrying later"
            );
            return Err(StoreError::from(SlowDown));
        }

        let mut deleted_a_version = false;
        for version in listing.versions_of(key) {
            if version.is_delete_marker {
                continue;
            }
            deleted_a_version = true;
            self.delete_object_version(key, version.version_id.as_deref())
                .await?;
        }

        if !deleted_a_version {
            self.delete_object_version(key, None).await?;
        }

        Ok(())
    }

    /// Delete one version of the object at `key`, or the unversioned object when `version` is
    /// `None`.
    async fn delete_object_version(
        &self,
        key: &str,
        version: Option<&str>,
    ) -> Result<(), StoreError> {
        self.s3
            .delete_object(self.bucket.as_str(), key, version)
            .await
            .map(|_| ())
            .map_err(|e| store_failure(e, "S3 delete object failed"))
    }

    async fn s3_head_object(&self, hash: Hash) -> Result<HeadObjectOutput, StoreError> {
        let mut dst = [0u8; 64];
        let output = self
            .s3
            .head_object(
                self.bucket.as_str(),
                lore_revision::util::to_hex_str(hash.data(), &mut dst),
            )
            .await
            .map_err(|e| {
                if names_no_object(&e, S3ErrorCode::is_not_found) {
                    return StoreError::from(AddressNotFound::from(Address::zero_context_hash(
                        hash,
                    )));
                }

                store_failure(e, "S3 head object failed")
            })?;
        Ok(output)
    }

    /// Read a fragment without its payload, from the object's object metadata.
    ///
    /// This is the one path that spends an S3 request purely on metadata, and it spends the
    /// cheapest one: `HeadObject` transfers no body. Reads that want the payload get the fragment
    /// for free on the `GetObject` response instead.
    async fn head_fragment(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let output = self.s3_head_object(hash).await?;

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
    #[lore_macro::test_pub]
    pub(crate) async fn load_state(&self, hash: Hash) -> Result<Option<FragmentState>, StoreError> {
        let item = to_item(FragmentStateEntry::key(hash)).map_err(|e| {
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
            .map_err(|e| store_failure(e, "DynamoDB fragment state read failed"))?
            .item
        else {
            return Ok(None);
        };

        let entry: FragmentStateEntry = from_item(av_map).map_err(|e| {
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

        let item = to_item(FragmentStateEntry::key(hash)).map_err(|e| {
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
            .map_err(|e| store_failure(e, "DynamoDB fragment metadata read failed"))?
            .item
            .map(from_item::<FragmentMetadataEntry>)
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
                if names_no_object(&e, S3ErrorCode::is_no_such_key) {
                    return StoreError::from(AddressNotFound::from(Address::zero_context_hash(
                        hash,
                    )));
                }

                store_failure(e, "S3 get object failed")
            })?;

        // The Content-Length header arrives with the response before the body is streamed.
        // Check it here to abort before reading a single byte of an oversized object.
        // Legacy S3 blobs had a Fragment struct prepended to the payload, so the maximum
        // legitimate object size is the threshold plus that prefix.
        let content_length: Option<usize> = output
            .content_length()
            .and_then(|l| usize::try_from(l).ok())
            .filter(|l| *l > 0);

        const MAX_OBJECT_SIZE: usize = FRAGMENT_SIZE_THRESHOLD + size_of::<Fragment>();
        if let Some(size) = content_length
            && size > MAX_OBJECT_SIZE
        {
            return Err(oversized_object(hash, size, MAX_OBJECT_SIZE));
        }

        let fragment = from_object_metadata(output.metadata());
        if let Ok(fragment) = &fragment {
            lore_storage::validate_fragment_size(fragment)?;
        }

        // Clamped to MAX_OBJECT_SIZE: legacy blobs carried a Fragment prefix that pushes their
        // size above FRAGMENT_SIZE_THRESHOLD, so capping at the threshold would under-allocate.
        let capacity = content_length.map_or(MAX_OBJECT_SIZE, |length| length.min(MAX_OBJECT_SIZE));

        let mut buffer = BytesMut::with_capacity(capacity);
        let mut read = 0_usize;
        while let Some(bytes) = output.body.next().await {
            let bytes = bytes.map_err(|e| {
                // An interrupted body is a transport failure, not a damaged object.
                warn!("Failed to read bytes from S3 response for key: {hash}: {e:?}");
                StoreError::from(SlowDown)
            })?;
            // A chunked reply declares no length, so only this bound covers it.
            let total = read.saturating_add(bytes.len());
            if total > MAX_OBJECT_SIZE {
                return Err(oversized_object(hash, total, MAX_OBJECT_SIZE));
            }

            read = total;
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
    #[lore_macro::test_pub]
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
        for reference in sub_fragments
            .iter()
            .filter(|reference| !reference.is_hole())
        {
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
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    /// Durable storage for every tenant at once, so a payload is only ever served to the partition
    /// that holds an association for it.
    fn isolates_partitions(&self) -> bool {
        true
    }

    /// The exact association, which is also all this store reads: it reports no wider than it
    /// serves.
    ///
    /// Stated rather than defaulted. The default reads `MatchPartition` off
    /// [`ImmutableStoreTrait::isolates_partitions`], on the reasoning that a store reporting a
    /// partition match hands the caller a level to act on with a `copy` instead of a transfer. This
    /// store declines to establish that level — telling an unassociated hash apart from an absent
    /// one costs a `Query` per address, which `query` explains — so it would be advertising a
    /// shortcut that never arrives.
    fn query_scope(&self) -> StoreMatch {
        StoreMatch::MatchFull
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
    #[tracing::instrument(name = "AwsImmutableStore::query" skip(self))]
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        // Timed around a call rather than around the body, unlike the other operations here: `?`
        // inside a `timed!` block returns from this function and skips the record, so an inline
        // body would report only the calls that succeeded.
        timed!(self.latency_histogram, &self.labels_query, {
            self.do_query_batch(partition, addresses, results).await
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
    ) -> Result<StoreGetData, StoreError> {
        timed!(self.latency_histogram, &self.labels_get_metadata, {
            let miss = StoreGetData::default();

            // The `HeadObject` goes out alongside the resolution rather than after it, the way
            // `get` overlaps its read: the answer this returns ends in that request, so waiting
            // for the resolution first would put two round trips in series on every hit. One
            // resolution is all there is to wait for — this store's read scope is the exact
            // association, so there is no wider level for a miss to fall back to.
            let mut query_fut = Box::pin(self.do_query(
                partition,
                address,
                &self.labels_get_metadata,
                SharedReadContext::GetMetadata,
            ));
            let mut head_fut = Box::pin(self.head_fragment(address.hash));

            let mut head_result = None;
            let query_output = loop {
                tokio::select! {
                    result = &mut query_fut => break result,
                    result = &mut head_fut, if head_result.is_none() => {
                        head_result = Some(result);
                    }
                }
            };

            // Resolution decides the answer, so its error wins and cancels the head.
            let (query_source, query_result) = query_output?;
            let match_made = query_result.match_made;

            if match_made == StoreMatch::MatchNone {
                return Ok(miss);
            }

            match query_source {
                // A legacy fragment carried its whole representation in the metadata row, so the
                // `HeadObject` has nothing to add and its result is dropped. Sparing it would mean
                // holding the common path back for the pre-cutover one, which is the wrong way
                // round.
                QueryResultSource::LegacyMetadata(fragment) => {
                    Ok(StoreGetData::metadata(fragment, match_made, partition))
                }
                QueryResultSource::State => {
                    let head_output = match head_result {
                        Some(result) => result,
                        None => head_fut.await,
                    };

                    match head_output {
                        Ok(fragment) => Ok(StoreGetData::metadata(fragment, match_made, partition)),
                        Err(e) if e.is_address_not_found() => {
                            self.report_missing_payload(address, &self.labels_get_metadata)
                                .await;
                            Ok(miss)
                        }
                        Err(e) => Err(e),
                    }
                }
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
                // Run both futures concurrently. The select! loop breaks as soon as exists
                // resolves. If load finishes first its result is stashed, and we keep waiting for
                // exists check.
                let exists_fut = self.exists(partition, address, SharedReadContext::Get);
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
                // If exists failed, its error is returned here; load_fut is dropped (canceled) on
                // the early return. Exists error takes priority over any load error.
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
                    self.report_missing_payload(address, &self.labels_get).await;
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
            content: None,
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
                    self.exists(partition, address, SharedReadContext::Put),
                    self.load_state_shared(address.hash, SharedReadContext::Put)
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
                        self.write_payload_and_state(address.hash, fragment, payload)
                            .await?;
                        self.associate_fragment(partition, address).await?;
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
        timed!(self.latency_histogram, &self.labels_obliterate, {
            // Note: given the importance of the work done here, and how relatively infrequently we
            // expect this to be invoked, the log output in this method is intentionally very
            // verbose.
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

            // The reference goes first, because that is the obligation: once this returns, this
            // partition does not name the content. Everything after it is reclamation.
            if state.is_obliteration() {
                info!(
                    "Fragment {address} is already being, or has already been, obliterated; \
                     deleting this partition's association only"
                );
                return self
                    .delete_association(partition, address)
                    .instrument(span.clone())
                    .await;
            }

            self.delete_association(partition, address)
                .instrument(span.clone())
                .await?;
            stats
                .num_fragments
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            Self::marked_step(|| self.take_mark(address.hash))
                .instrument(span.clone())
                .await?;
            info!("Acquired obliteration mark for {address}");

            // Again, now the mark is up: a writer that raced the first delete recreated the
            // association while the hash still looked live, and would otherwise outlive the
            // obliteration. One that associates after this point sees the mark and is turned away,
            // or is re-storing content of its own - which is a new write, not this one surviving.
            Self::marked_step(|| self.delete_association(partition, address))
                .instrument(span.clone())
                .await?;

            tokio::time::sleep(self.obliteration_drain).await;

            info!("Association deleted, re-checking for other associations...");
            if Self::marked_step(|| self.has_associations(address.hash))
                .instrument(span.clone())
                .await?
            {
                info!("Fragment still associated, releasing the obliteration mark");
                return Self::marked_step(|| self.clear_mark(address.hash, FragmentState::Stored))
                    .instrument(span.clone())
                    .await
                    .inspect_err(|e| {
                        warn!({PARTITION_ID} = %partition, {ADDRESS} = %address, error = ?e, "Failed to release the obliteration mark");
                    });
            }

            Self::marked_step(|| {
                self.clone()
                    .obliterate_sub_fragments(partition, address, stats.clone())
            })
            .instrument(span.clone())
            .await?;

            Self::marked_step(|| self.delete_payload(address.hash))
                .instrument(span.clone())
                .await?;

            stats
                .num_payloads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            Self::marked_step(|| self.clear_mark(address.hash, FragmentState::Obliterated))
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
        // S3 itself tracks the destination object's existence as the source of durability, and it
        // has no replicas of its own to pass the copy on to, so neither flag applies here.
        _behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        // The destination tuple shares the source's hash but takes the caller's chosen context
        // — that is the only field the storage trait allows the caller to pivot on a copy.
        let destination_address = Address {
            hash: source_address.hash,
            context: destination_context,
        };
        timed!(self.latency_histogram, &self.labels_copy, {
            let present = if source_address.context.is_zero() {
                self.has_partition_association(source_partition, source_address.hash)
                    .await?
            } else {
                self.exists(source_partition, source_address, SharedReadContext::Copy)
                    .await?
            };
            if !present {
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

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
        Ok(())
    }

    fn max_query_batch(&self) -> Option<usize> {
        // DynamoDB batch size cannot exceed 100
        Some(crate::dynamodb::BATCH_GET_ITEM_MAX_COUNT)
    }
}

/// Access to the store's flights, so a test can hold a read in flight.
#[cfg(feature = "test-util")]
pub mod test_util {
    use lore_base::types::Address;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_storage::StoreError;

    use super::AwsImmutableStore;
    use super::FragmentState;
    use crate::store::single_flight::SingleFlight;

    impl AwsImmutableStore {
        pub fn association_reads(
            &self,
        ) -> &SingleFlight<(Partition, Address), Result<bool, StoreError>> {
            &self.association_reads
        }

        pub fn state_reads(
            &self,
        ) -> &SingleFlight<Hash, Result<Option<FragmentState>, StoreError>> {
            &self.state_reads
        }
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
