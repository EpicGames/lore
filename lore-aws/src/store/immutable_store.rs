// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::string::ToString;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::Select;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_smithy_types::DateTime;
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

/// Lower bound on how long to wait for an in-flight upload to publish before treating the object
/// it left behind as abandoned.
const MIN_ABANDONED_GRACE_MILLIS: u64 = 100;

/// Lower bound on the obliteration drain, regardless of how the `DynamoDB` timeout is configured.
const MIN_OBLITERATION_DRAIN_MILLIS: u64 = 100;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whether an object stored at `last_modified` has gone unpublished long enough that the writer
/// which stored it must be gone rather than still finishing.
///
/// Deliberately one-sided: without a usable timestamp, or with one that looks like the future,
/// the object is treated as live. Concluding too early is not a correctness problem — reclaiming
/// is single-winner and only a reclaimer may overwrite metadata — but it does waste another
/// writer's upload, so the doubt is resolved in their favour.
///
/// The age is measured against the local clock, so the threshold wants to stay comfortably above
/// any skew between this host and S3.
fn is_abandoned(last_modified: Option<&DateTime>, threshold_millis: u64) -> bool {
    let Some(stored_at_millis) = last_modified.and_then(datetime_millis) else {
        // An object that cannot be dated can never be judged abandoned, so it will never be
        // reclaimed and every put for this hash backs off indefinitely. Real S3 always reports
        // this; an implementation that does not turns a recoverable orphan into an unwritable
        // hash, which is worth saying out loud rather than leaving to look like contention.
        error!(
            "Stored object reports no usable last-modified time, so it can never be reclaimed \
             if its writer abandoned it"
        );

        return false;
    };

    let now = now_millis();

    if stored_at_millis > now {
        warn!(
            ahead_millis = stored_at_millis - now,
            "Stored object is dated in the future; check this host's clock against S3, as \
             reclaiming abandoned uploads depends on the two agreeing"
        );

        return false;
    }

    now - stored_at_millis >= threshold_millis
}

/// Milliseconds since the epoch, or `None` for a time that predates it.
///
/// Keeps sub-second precision, so a threshold below a second means what it says.
fn datetime_millis(time: &DateTime) -> Option<u64> {
    let seconds = u64::try_from(time.secs()).ok()?;

    Some(seconds.saturating_mul(1_000) + u64::from(time.subsec_nanos()) / 1_000_000)
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
    /// How long to keep waiting for another writer to publish an object it has already uploaded
    /// before treating that object as abandoned and recovering it. Defaults to a multiple of the
    /// S3 request timeout, so a merely slow writer is given time to finish rather than having its
    /// upload adopted.
    pub abandoned_upload_grace_millis: Option<u64>,
    /// How long an obliteration waits, after marking a row, for puts that read it beforehand to
    /// finish writing their association. Sized above the `DynamoDB` request timeout so such a write
    /// has either landed or failed by the time the references are counted again.
    pub obliteration_drain_millis: Option<u64>,
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
            abandoned_upload_grace_millis: None,
            obliteration_drain_millis: None,
        }
    }

    /// Resolve how long a writer waits for someone else's upload to be published. Sized above the
    /// S3 request timeout so a live writer has finished by the time its object is treated as
    /// abandoned; recovery is correct either way, this only avoids doing it needlessly.
    fn abandoned_grace_millis(&self) -> u64 {
        self.abandoned_upload_grace_millis
            .unwrap_or_else(|| self.s3.timeout_millis.saturating_mul(4))
            // `max` raises anything below the floor up to it.
            .max(MIN_ABANDONED_GRACE_MILLIS)
    }

    /// Resolve the obliteration drain. Obliteration is rare and not latency sensitive, so this is
    /// sized generously: an association write that had already begun must have completed or timed
    /// out before the references are counted again.
    fn obliteration_drain_millis(&self) -> u64 {
        self.obliteration_drain_millis
            .unwrap_or_else(|| self.dynamodb.timeout_millis.saturating_mul(4))
            // `max` raises anything below the floor up to it.
            .max(MIN_OBLITERATION_DRAIN_MILLIS)
    }
}

/// Counts payloads whose metadata says they are stored but whose object is not in S3.
///
/// Should always be zero. A non-zero value means content has been lost underneath the store —
/// see [`AwsImmutableStore`] for why nothing repairs it automatically.
pub const METRICS_MISSING_PAYLOAD_METRIC_NAME: &str = "store.immutable.missing_payload";

pub const FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE: &str = "hash";
pub const FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE: &str = "repository_context";

/// How many times `put` re-probes after losing a race. Each retry is one extra metadata read;
/// the bound stops a contended hash from spinning here instead of returning to the caller, which
/// can retry with its own backoff.
const PUT_MAX_ATTEMPTS: usize = 3;

/// What a `put` should do, given what its probe observed.
#[derive(Debug, PartialEq)]
enum PutAction {
    /// The exact association already exists against durable content; nothing to write.
    Done,
    /// The payload is already durable, in this or another partition. Record the association and
    /// skip the upload.
    Deduplicate(Fragment),
    /// Store the bytes and publish them, with the publish conditioned on this.
    Upload(MetadataWriteCondition),
    /// The caller did not supply the bytes and nothing here entitles it to skip them.
    PayloadRequired,
    /// The payload is marked for obliteration. The mark is transient, so this is a back-off
    /// rather than a failure.
    Obliterating,
    /// Different content is already stored under this hash.
    Collision,
}

/// Decide what a put should do from the probed association and metadata state.
///
/// Deduplication hinges on [`MetadataState::Committed`] meaning the payload is durable in S3:
/// rows are only committed after their upload succeeded, so a committed row lets this writer
/// record a reference instead of re-uploading bytes the server already holds. Crucially the
/// *stored* fragment is what gets referenced, never the incoming one — the S3 object is whatever
/// the original writer stored, so adopting an incoming fragment that described a different
/// representation would leave the metadata contradicting the bytes.
fn decide_put(
    fragment: Fragment,
    associated: bool,
    stored: Option<Fragment>,
    force_write: bool,
    has_payload: bool,
) -> PutAction {
    match stored {
        // Nothing stored, so this writer has to supply the bytes. Note this is also how a lost
        // metadata row heals: the association may well exist already, but without metadata the
        // payload is not readable, so it is stored and published again.
        None => PutAction::Upload(MetadataWriteCondition::Absent),

        Some(stored) => {
            // Checked ahead of `force_write`, because an obliteration in progress holds a lock on
            // this row. Overwriting it would release that lock underneath the obliteration and
            // resurrect content that is being deleted.
            if stored.flags & FragmentFlags::PayloadObliterating
                == FragmentFlags::PayloadObliterating
            {
                PutAction::Obliterating
            } else if force_write {
                PutAction::Upload(MetadataWriteCondition::Unchanged(stored))
            } else if stored.flags & FragmentFlags::PayloadObliterated
                == FragmentFlags::PayloadObliterated
            {
                // A tombstone: the payload was deleted, so the bytes must be stored again.
                PutAction::Upload(MetadataWriteCondition::Unchanged(stored))
            } else if fragment.size_content != stored.size_content {
                PutAction::Collision
            } else if associated {
                PutAction::Done
            } else if has_payload {
                // Attaching content the caller does not already reference means presenting the
                // bytes for it. The upload is what deduplication skips, not that requirement:
                // a hash on its own is not evidence the caller holds the content, and treating
                // it as such would let one be attached to a partition that never had it.
                PutAction::Deduplicate(stored)
            } else {
                PutAction::PayloadRequired
            }
        }
    }
}

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

/// Condition parts asserting that a metadata row still describes exactly `expected`.
fn committed_metadata_condition_parts(expected: Fragment) -> ConditionParts {
    ConditionParts {
        condition_expression:
            "#flags = :flags AND #size_payload = :size_payload AND #size_content = :size_content"
                .to_string(),
        expression_names: HashMap::from([
            ("#flags".to_string(), "flags".to_string()),
            ("#size_payload".to_string(), "size_payload".to_string()),
            ("#size_content".to_string(), "size_content".to_string()),
        ]),
        expression_values: HashMap::from([
            (
                ":flags".to_string(),
                AttributeValue::N(expected.flags.to_string()),
            ),
            (
                ":size_payload".to_string(),
                AttributeValue::N(expected.size_payload.to_string()),
            ),
            (
                ":size_content".to_string(),
                AttributeValue::N(expected.size_content.to_string()),
            ),
        ]),
    }
}

/// Whether a conditional put failed its condition, as opposed to failing outright. Callers treat
/// this as "another writer won the race", not as an error.
fn is_conditional_check_failed(error: &AwsError<DynamoDbSdkError<PutItemError>>) -> bool {
    let AwsError::AwsSdkError(sdk_error) = error else {
        return false;
    };

    sdk_error
        .as_service_error()
        .is_some_and(PutItemError::is_conditional_check_failed_exception)
}

#[derive(Debug, PartialEq)]
struct UpdateMetadataCondition(Fragment);

impl DynamoDbPutCondition for UpdateMetadataCondition {
    fn into_parts(self) -> ConditionParts {
        committed_metadata_condition_parts(self.0)
    }
}

/// Guards publishing a payload's metadata.
///
/// Publishing is only ever done by the writer whose own conditional upload created the object,
/// or by one that recovered the object's representation from the object itself. The condition
/// stops that publish from overwriting a row that changed in the meantime — most importantly an
/// obliteration lock, which must not be cleared by a concurrent write.
#[derive(Debug, PartialEq)]
enum MetadataWriteCondition {
    /// No metadata row exists for this hash at all.
    Absent,
    /// The row still describes exactly this fragment.
    Unchanged(Fragment),
}

impl DynamoDbPutCondition for MetadataWriteCondition {
    fn into_parts(self) -> ConditionParts {
        match self {
            MetadataWriteCondition::Absent => ConditionParts {
                condition_expression: "attribute_not_exists(#hash)".to_string(),
                expression_names: HashMap::from([("#hash".to_string(), "hash".to_string())]),
                expression_values: HashMap::new(),
            },
            MetadataWriteCondition::Unchanged(expected) => {
                committed_metadata_condition_parts(expected)
            }
        }
    }
}

static STORE_ATTRIBUTES: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("store", "aws")]);

type BatchTaskResult = Result<(usize, StoreMatch), (usize, StoreError)>;

/// Result of an upload: whether this writer is the one that created the object.
#[derive(Debug, PartialEq, Eq)]
enum UploadOutcome {
    /// This writer's bytes are now the object's contents.
    Stored,
    /// An object this writer did not create is already under the key.
    AlreadyPresent,
}

/// What became of an object found under a key this writer does not own.
#[derive(Debug, PartialEq, Eq)]
enum UnpublishedObject {
    /// A published row accounts for it, so it is durable and described.
    Published,
    /// An obliteration owns the hash, so this object should not exist at all. A tombstone is a
    /// row, but it does not account for an object.
    ObliteratedRemnant,
    /// Stored too recently to conclude anything: its writer is most likely still finishing.
    InFlight,
    /// Stored long enough ago that its writer would have published by now. The tag identifies
    /// the object as observed, so reclaiming it can be made single-winner.
    Abandoned(Option<String>),
}

/// Whether this writer's bytes displaced someone else's.
///
/// A conditional upload cannot displace anything, so its bytes stay canonical until an
/// obliteration or a reclaim removes them. An unconditional one replaces whatever was there,
/// which is what decides who may overwrite whose metadata when a publish is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    /// Written only because the key was free.
    Exclusive,
    /// Written over whatever was there.
    Displacing,
}

/// How far a publish got before the row moved underneath it.
#[derive(Debug, PartialEq, Eq)]
enum PublishOutcome {
    Published,
    /// An obliteration took the row. It deletes the payload itself, so there is nothing to do.
    Obliterating,
    /// Another writer owns the hash now, and these bytes are no longer known to be the stored
    /// ones, so publishing over it would be a guess.
    Superseded,
    /// The row kept changing; the publish never landed.
    Contended,
}

/// How many times a publish re-reads and re-conditions before giving up. Only the metadata write
/// is retried: the bytes are already stored, so re-uploading them buys nothing.
const PUBLISH_MAX_ATTEMPTS: usize = 3;

/// Classify a failed upload, keeping the underlying error attached so the cause survives into
/// the error chain rather than only into the log line.
fn upload_error<E>(key: &str, error: AwsError<E>) -> StoreError
where
    AwsError<E>: std::error::Error + Send + Sync + 'static,
{
    warn!(%key, ?error, "Failed to write payload");

    if matches!(error, AwsError::AwsSdkError(_)) {
        StoreError::from(SlowDown)
    } else {
        StoreError::internal_with_context(error, "S3 put object failed")
    }
}

struct GetS3objectContentsOutput {
    read: usize,
    bytes: BytesMut,
}

/// Fragment storage backed by S3 for payloads and `DynamoDB` for the metadata describing them.
///
/// # Published metadata is authoritative
///
/// A published metadata row is taken as proof that the payload it describes is in S3. That is
/// what lets a put deduplicate — recording a reference to content another partition already
/// stored, without re-uploading it or consulting S3 at all — and it holds because metadata is
/// only ever published after the upload that stored those bytes succeeded.
///
/// The assumption is not verified on the read or deduplication paths, deliberately: checking it
/// would cost an S3 request per put and give back exactly what deduplication buys. So if an
/// object is removed from S3 while its metadata row survives — a lifecycle rule, a direct
/// deletion, or S3 and `DynamoDB` being restored to different points in time — the store cannot
/// tell, and:
///
/// - reads of that hash fail from every partition referencing it, not only the one that wrote it;
/// - further puts of the same content deduplicate onto it, spreading references to a payload that
///   cannot be read;
/// - nothing repairs it. Re-writing the content does not, because a published row makes the put
///   deduplicate rather than upload.
///
/// This is an operational failure to be recovered by whatever restores the object, not a state
/// the store resolves on its own. It is reported rather than hidden: see
/// [`METRICS_MISSING_PAYLOAD_METRIC_NAME`], which counts reads that find published metadata with
/// no object behind it and should never be non-zero.
pub struct AwsImmutableStore {
    s3: S3,
    dynamodb: DynamoDb,
    task_queue: TaskQueue<BatchTaskResult>,
    bucket: String,
    fragments_table_name: Arc<str>,
    metadata_table_name: Arc<str>,
    force_write: bool,
    abandoned_grace_millis: u64,
    obliteration_drain_millis: u64,
    latency_histogram: Histogram<f64>,
    missing_payload_counter: Counter<u64>,
    labels_missing_payload: LabelArray,
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
        let missing_payload_counter = provider.counter(METRICS_MISSING_PAYLOAD_METRIC_NAME);
        let labels_missing_payload = provider.get_labels_for_operation_context("get");
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
            abandoned_grace_millis: settings.abandoned_grace_millis(),
            obliteration_drain_millis: settings.obliteration_drain_millis(),
            latency_histogram,
            missing_payload_counter,
            labels_missing_payload,
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

    /// Publish the metadata describing a payload that is already in S3, making it visible.
    ///
    /// Only ever called by a writer whose own upload put the bytes under the key — a conditional
    /// upload that created the object, a reclaim that replaced an abandoned one, or a
    /// `force_write` that replaced it deliberately — so the fragment published here always
    /// describes the bytes actually stored.
    ///
    /// `Ok(false)` means the condition rejected the write and the caller should re-probe.
    async fn publish_metadata(
        &self,
        hash: Hash,
        fragment: Fragment,
        condition: MetadataWriteCondition,
    ) -> Result<bool, StoreError> {
        let item = serde_dynamo::to_item(FragmentMetadataEntry::new(hash).with_fragment(fragment))
            .map_err(|e| {
                warn!(%hash, ?e, "Failed to serialize metadata entry");
                StoreError::internal_with_context(e, "Failed to serialize metadata for DynamoDB")
            })?;

        match self
            .dynamodb
            .put_item_conditional(&self.metadata_table_name, item, condition)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if is_conditional_check_failed(&e) => {
                debug!(%hash, "Metadata changed before this write could publish");
                Ok(false)
            }
            Err(e) => {
                warn!(%hash, ?e, "Failed to publish payload metadata");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    Err(StoreError::from(SlowDown))
                } else {
                    Err(StoreError::internal_with_context(
                        e,
                        "DynamoDB metadata publish failed",
                    ))
                }
            }
        }
    }

    /// Publish, re-reading and re-conditioning if the row moves underneath us.
    ///
    /// A rejected publish means the row changed since it was probed, not that the bytes are
    /// wrong, so the right response is to re-condition against what is there now rather than
    /// unwind and store the payload again.
    async fn publish_converging(
        &self,
        hash: Hash,
        fragment: Fragment,
        mut condition: MetadataWriteCondition,
        mode: WriteMode,
    ) -> Result<PublishOutcome, StoreError> {
        for _ in 0..PUBLISH_MAX_ATTEMPTS {
            if self.publish_metadata(hash, fragment, condition).await? {
                return Ok(PublishOutcome::Published);
            }

            let current = self.metadata_lookup(hash).await?;

            if matches!(current, Some(f) if f.flags & FragmentFlags::PayloadObliteration != 0) {
                return Ok(PublishOutcome::Obliterating);
            }

            // Only a writer that displaced what was under the key may overwrite another writer's
            // metadata, because only it knows the stored bytes are its own. A writer whose upload
            // was conditional cannot know that: its bytes may since have been reclaimed, and
            // publishing over the reclaimer would describe their object with this writer's
            // fragment.
            if mode == WriteMode::Exclusive {
                return Ok(PublishOutcome::Superseded);
            }

            condition = match current {
                Some(current) => MetadataWriteCondition::Unchanged(current),
                None => MetadataWriteCondition::Absent,
            };
        }

        Ok(PublishOutcome::Contended)
    }

    /// Record that `(repository, context)` references a payload, without guarding the metadata
    /// row.
    ///
    /// Obliteration marks the metadata row before removing a payload and counts references again
    /// afterwards, so a put that saw no mark is either counted by that second pass or has already
    /// backed off. Nothing here needs to be conditional on the row.
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

        self.dynamodb
            .put_item(&self.fragments_table_name, item)
            .await
            .map_err(|e| {
                warn!({REPOSITORY_ID} = %repository, {ADDRESS} = %address, error = ?e, "Failed to put item while storing fragment association");
                if matches!(&e, AwsError::AwsSdkError(_)) {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::internal_with_context(
                        e,
                        "DynamoDB fragment association write failed",
                    )
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

    /// Store a fragment, deduplicating the payload against content the server already holds.
    ///
    /// The probe is two strongly consistent single-item reads on different tables, issued
    /// together so they cost one round trip. The metadata table is keyed by hash alone, so
    /// asking whether a payload is already durable is a point lookup whose cost does not grow
    /// with how many partitions reference it — and S3 is never consulted to answer it.
    ///
    /// ```text
    /// OK        returned success        RE-PROBE  loop back to the probe (max 3, then SLOWDOWN)
    /// ERROR     returned failure        SLOWDOWN  returned to the caller to retry later
    ///
    /// PROBE  GetItem fragments  PK=hash SK=repo|ctx  ┐ concurrent
    ///        GetItem metadata   PK=hash              ┘ no S3
    ///   │
    ///   ├─ no metadata row ...................................... UPLOAD  cond: absent
    ///   ├─ flags: Obliterating .................................. SLOWDOWN
    ///   ├─ force_write .......................................... UPLOAD  cond: unchanged
    ///   ├─ flags: Obliterated ................................... UPLOAD  cond: unchanged
    ///   ├─ size_content differs ................................. ERROR  hash collision
    ///   ├─ association present .................................. OK  (no writes)
    ///   ├─ payload supplied ..................................... DEDUPLICATE
    ///   └─ no payload ........................................... ERROR  payload required
    ///
    /// DEDUPLICATE  PutItem fragments ............................ OK
    ///   No S3 request: a published row already means the payload is durable. The stored
    ///   fragment is referenced, never the incoming one.
    ///
    /// UPLOAD  PutObject S3   If-None-Match (or unconditional under force_write)
    ///   ├─ stored ............................................... PUBLISH
    ///   └─ 412, the key is taken ................................ RESOLVE
    ///
    /// RESOLVE  GetItem metadata
    ///   ├─ published ............................................ RE-PROBE
    ///   ├─ obliteration flags → discard the object .............. RE-PROBE
    ///   └─ absent → HeadObject S3   ETag + Last-Modified
    ///        ├─ younger than the threshold ...................... SLOWDOWN
    ///        └─ older → PutObject S3  If-Match  (single winner)
    ///             ├─ 412, lost the reclaim ...................... RE-PROBE
    ///             └─ stored ..................................... PUBLISH
    ///
    /// PUBLISH  PutItem metadata  conditional
    ///   ├─ accepted → PutItem fragments ......................... OK
    ///   └─ rejected → GetItem metadata
    ///        ├─ obliteration flags → discard the object ......... RE-PROBE
    ///        ├─ Exclusive (our upload was conditional) .......... RE-PROBE
    ///        └─ Displacing → re-condition and retry (max 3)
    ///             ├─ accepted ................................... OK
    ///             └─ exhausted .................................. ERROR  (logged loudly)
    /// ```
    ///
    /// `SLOWDOWN` means nothing will change within this request — another writer owns the
    /// outcome, so the caller has to come back. `RE-PROBE` means the state changed underneath
    /// us and the correct action is now a different one, which re-reading resolves without
    /// troubling the caller.
    ///
    /// The two `WriteMode` cases decide who may overwrite whose metadata after a rejected
    /// publish. `Exclusive` means the upload was conditional, so these bytes may since have been
    /// reclaimed and this writer cannot vouch for what is stored; it stands down. `Displacing`
    /// means a reclaim or a `force_write` replaced whatever was there, so the stored bytes are
    /// known to be this writer's.
    async fn put_deduplicated(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), StoreError> {
        for _ in 0..PUT_MAX_ATTEMPTS {
            // Both reads are single-item and hit different tables, so issuing them together costs
            // one round trip rather than two. The metadata table is keyed by hash alone, which is
            // what keeps the deduplication probe independent of how many partitions reference the
            // content: no query, no scan, and S3 is never consulted.
            let association = FragmentsEntry::new(repository, address);
            let (associated, state) = tokio::join!(
                self.exists_exact(&association),
                self.metadata_lookup(address.hash),
            );

            match decide_put(
                fragment,
                associated?,
                state?,
                self.force_write,
                payload.is_some(),
            ) {
                PutAction::Done => return Ok(()),

                PutAction::PayloadRequired => {
                    return Err(StoreError::internal("Payload buffer required"));
                }

                // Backing off is what keeps a new reference from appearing while the payload is
                // being removed, which is what lets the obliteration count references without a
                // transaction to serialise against.
                PutAction::Obliterating => {
                    debug!(%address, "Payload is marked for obliteration; backing off");
                    return Err(StoreError::from(SlowDown));
                }

                PutAction::Collision => return Err(StoreError::internal("Hash collision")),

                // The payload is already durable, so this only has to record the reference.
                // Obliteration marks the row before removing a payload and counts references
                // again afterwards, so a put that saw no mark is either counted or backed off.
                PutAction::Deduplicate(_) => {
                    self.associate_fragment(repository, address).await?;

                    return Ok(());
                }

                PutAction::Upload(condition) => {
                    let Some(payload) = payload.clone() else {
                        return Err(StoreError::internal("Payload buffer required"));
                    };

                    if self
                        .write_payload(repository, address, fragment, payload, condition)
                        .await?
                    {
                        return Ok(());
                    }
                }
            }
        }

        debug!(%address, "Gave up storing after repeatedly losing the race for the payload");
        Err(StoreError::from(SlowDown))
    }

    /// Store a payload and publish the metadata describing it.
    ///
    /// The upload is conditional on the key being absent, so exactly one writer can ever create
    /// the bytes for a hash. That is the whole of the mutual exclusion: no lock is needed,
    /// because S3 itself arbitrates. Metadata is published only after *this* writer's own upload
    /// created the object, so the published fragment always describes the bytes that are there —
    /// which is what stops two writers holding the same content in different representations
    /// (different compression, say) from leaving the pair disagreeing.
    ///
    /// `Ok(false)` means another writer got there first and the caller should re-probe.
    async fn write_payload(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
        payload: Bytes,
        publish_with: MetadataWriteCondition,
    ) -> Result<bool, StoreError> {
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
        let key = lore_revision::util::to_hex_str(address.hash.data(), &mut dst);

        // `force_write` deliberately replaces whatever is stored, so it writes unconditionally;
        // it is an operator override rather than part of the concurrent write protocol. Replacing
        // in place keeps it from ever leaving the key empty, which deleting first would.
        let (stored, mut mode) = if self.force_write {
            (self.upload(key, &payload).await?, WriteMode::Displacing)
        } else {
            (
                self.upload_if_absent(key, &payload).await?,
                WriteMode::Exclusive,
            )
        };

        let (published_fragment, condition) = match stored {
            UploadOutcome::Stored => (fragment, publish_with),

            // The key is taken by an object this writer did not create, so its bytes are not the
            // ones `fragment` describes and publishing `fragment` would tear the pair.
            UploadOutcome::AlreadyPresent => {
                match self.resolve_unpublished_object(address.hash).await? {
                    // Another writer published while we were uploading. Nothing to store.
                    UnpublishedObject::Published => return Ok(false),

                    // The object outlived the content it belongs to. Nothing references it and
                    // nothing can adopt it, so removing it both unwedges the hash — the next
                    // attempt's conditional upload can finally succeed — and stops obliterated
                    // bytes from sitting in S3 indefinitely.
                    UnpublishedObject::ObliteratedRemnant => {
                        self.discard_orphaned_object(address.hash).await;
                        return Ok(false);
                    }

                    // Another writer stored this recently and has not published yet. Its
                    // representation is the one that will win, so this writer backs off and lets
                    // the caller come back rather than holding the request or racing it.
                    UnpublishedObject::InFlight => {
                        debug!(%address, "Upload is not published yet; backing off");
                        return Err(StoreError::from(SlowDown));
                    }

                    // Long enough unpublished that the writer that stored it is gone. Take the
                    // key over with bytes this writer can vouch for, rather than trying to work
                    // out what the abandoned ones were.
                    UnpublishedObject::Abandoned(etag) => {
                        if !self.reclaim_object(key, &payload, etag.as_deref()).await? {
                            debug!(%address, "Lost the race to reclaim the abandoned object");
                            return Ok(false);
                        }

                        info!(%address, "Reclaimed an abandoned object");
                        mode = WriteMode::Displacing;

                        (fragment, MetadataWriteCondition::Absent)
                    }
                }
            }
        };

        match self
            .publish_converging(address.hash, published_fragment, condition, mode)
            .await?
        {
            PublishOutcome::Published => {}

            // Every path that reaches here stored this writer's bytes: either the conditional
            // upload created the object, or the reclaim replaced it. Anything else returned
            // earlier. So the object is always this writer's to withdraw, and leaving it would
            // put content an obliteration is deleting back into S3.
            PublishOutcome::Obliterating => {
                self.discard_orphaned_object(address.hash).await;

                return Ok(false);
            }

            // An unconditional overwrite already replaced the stored bytes, so giving up here
            // leaves them described by another writer's metadata and nothing will repair it.
            // That has to surface as a failure an operator can see, not as a retry hint.
            PublishOutcome::Contended if mode == WriteMode::Displacing => {
                error!(
                    "Storing {address} replaced the stored payload but could not publish its \
                     metadata; the stored bytes and the published metadata may disagree"
                );
                return Err(StoreError::internal(format!(
                    "Failed to publish metadata for replaced payload {address}"
                )));
            }

            // Someone else owns the hash now, or the row would not settle. Either way the
            // conditional upload left any existing object untouched, so there is nothing to
            // repair and the caller can simply re-probe.
            PublishOutcome::Superseded | PublishOutcome::Contended => return Ok(false),
        }

        self.associate_fragment(repository, address)
            .await
            .map(|()| true)
    }

    /// Remove an object this writer created but could not publish, when an obliteration owns the
    /// hash.
    ///
    /// Obliteration marks the row before it deletes the payload, so a put that slips in between
    /// that delete and the final tombstone would otherwise leave the content back in S3 —
    /// unreferenced and unreadable, but present, which is not what "permanently delete" should
    /// mean. Re-reading the row is what makes removing it safe: it only proceeds while an
    /// obliteration holds the hash, which is exactly the case where nothing else can have adopted
    /// these bytes.
    async fn discard_orphaned_object(&self, hash: Hash) {
        let obliterating = matches!(
            self.metadata_lookup(hash).await,
            Ok(Some(fragment)) if fragment.flags & FragmentFlags::PayloadObliteration != 0
        );

        if !obliterating {
            return;
        }

        info!(%hash, "Discarding an orphaned object left behind by an obliteration");

        if let Err(e) = self.delete_payload(hash).await {
            warn!(%hash, ?e, "Failed to discard the unpublished object");
        }
    }

    /// Decide what to do about an object that exists under a key this writer does not own.
    ///
    /// The overwhelmingly likely explanation is a writer that is merely slow, so an object is
    /// only treated as abandoned once it has gone unpublished for longer than the threshold.
    /// Concluding that too early is not a correctness problem — reclaiming is single-winner and
    /// only a reclaimer may overwrite another writer's metadata — but it throws away a live
    /// writer's upload, so the doubt is resolved in their favour.
    async fn resolve_unpublished_object(
        &self,
        hash: Hash,
    ) -> Result<UnpublishedObject, StoreError> {
        match self.metadata_lookup(hash).await? {
            // A published row accounts for these bytes.
            Some(fragment) if fragment.flags & FragmentFlags::PayloadObliteration == 0 => {
                return Ok(UnpublishedObject::Published);
            }
            // A tombstone is a row, but it does not account for an object: obliteration deletes
            // the payload, so anything still under the key outlived its content.
            Some(_) => return Ok(UnpublishedObject::ObliteratedRemnant),
            None => {}
        }

        let mut dst = [0u8; 64];
        let key = lore_revision::util::to_hex_str(hash.data(), &mut dst);

        let object = self
            .s3
            .head_object(self.bucket.as_str(), key)
            .await
            .map_err(|e| {
                warn!(%hash, ?e, "Failed to inspect the unpublished object");
                StoreError::from(SlowDown)
            })?;

        // How long the object has been sitting unpublished is what separates a writer that has
        // died from one that is still finishing, and S3 already knows it. Asking is what keeps
        // this decision out of the request: waiting here would hold the caller for the whole
        // threshold to learn something a timestamp answers immediately.
        if !is_abandoned(object.last_modified(), self.abandoned_grace_millis) {
            return Ok(UnpublishedObject::InFlight);
        }

        debug!(%hash, "Object has gone unpublished long enough to be abandoned");

        Ok(UnpublishedObject::Abandoned(
            object.e_tag().map(ToString::to_string),
        ))
    }

    /// Take over an object whose writer never published it.
    ///
    /// Conditioned on the object still being the one just inspected, so exactly one writer can
    /// reclaim a given object: a second one racing for it is rejected and re-probes instead of
    /// replacing bytes the first is about to publish.
    ///
    /// `Ok(false)` means the reclaim was lost.
    async fn reclaim_object(
        &self,
        key: &str,
        payload: &Bytes,
        etag: Option<&str>,
    ) -> Result<bool, StoreError> {
        let Some(etag) = etag else {
            // Nothing to condition on. Only reachable if S3 returned no entity tag, and the
            // exposure is the one `force_write` already accepts.
            warn!(%key, "Reclaiming unconditionally: the stored object reported no entity tag");
            self.upload(key, payload).await?;

            return Ok(true);
        };

        match self
            .s3
            .put_object_if_match(self.bucket.as_str(), key, payload.to_vec(), etag)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if crate::s3::is_precondition_failed(&e) => Ok(false),
            Err(e) => Err(upload_error(key, e)),
        }
    }

    /// A single conditional upload.
    async fn upload_if_absent(
        &self,
        key: &str,
        payload: &Bytes,
    ) -> Result<UploadOutcome, StoreError> {
        match self
            .s3
            .put_object_if_absent(self.bucket.as_str(), key, payload.to_vec())
            .await
        {
            Ok(_) => Ok(UploadOutcome::Stored),
            Err(e) if crate::s3::is_precondition_failed(&e) => Ok(UploadOutcome::AlreadyPresent),
            Err(e) => Err(upload_error(key, e)),
        }
    }

    /// An unconditional upload, replacing whatever is stored under the key.
    async fn upload(&self, key: &str, payload: &Bytes) -> Result<UploadOutcome, StoreError> {
        self.s3
            .put_object(self.bucket.as_str(), key, payload.to_vec())
            .await
            .map(|_| UploadOutcome::Stored)
            .map_err(|e| upload_error(key, e))
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

        // Only a tombstone means the payload is gone. A row that is merely marked still has its
        // payload — it is removed after the references are counted — so refusing reads there
        // would hide content from every partition holding it, and would hide it permanently if
        // the obliteration that set the mark never finished.
        if (metadata.flags & FragmentFlags::PayloadObliterated) != 0 {
            return Err(StoreError::from(AddressNotFound::from(
                Address::zero_context_hash(hash),
            )));
        };

        Ok(metadata)
    }

    /// Read the raw metadata row for `hash`. A missing row is `Ok(None)` rather than an error, so
    /// callers can distinguish "nothing stored" from "the read failed".
    async fn metadata_entry(
        &self,
        hash: Hash,
    ) -> Result<Option<FragmentMetadataEntry>, StoreError> {
        let key = serde_dynamo::to_item(FragmentMetadataEntry::new(hash)).map_err(|e| {
            warn!("Failed to serialize fragment metadata entry for {hash}: {e:?}");
            StoreError::internal_with_context(
                e,
                "Failed to serialize fragment entry for DynamoDB metadata load",
            )
        })?;

        let item = self
            .dynamodb
            .get_item(
                &self.metadata_table_name,
                key,
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
            .item;

        match item {
            Some(av_map) => serde_dynamo::from_item(av_map).map(Some).map_err(|e| {
                warn!("Failed to deserialize fragment metadata: {e:?}");
                StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
            }),
            None => Ok(None),
        }
    }

    /// Read the metadata row for `hash` without treating absence as an error. This is the
    /// deduplication probe: a published row proves the payload is durable in S3, because
    /// metadata is only ever published after the upload that stored those bytes succeeded.
    ///
    /// One strongly consistent `GetItem` against a table keyed by hash alone, so the cost does
    /// not grow with how many partitions reference the content, and S3 is never consulted.
    async fn metadata_lookup(&self, hash: Hash) -> Result<Option<Fragment>, StoreError> {
        Ok(self
            .metadata_entry(hash)
            .await?
            .and_then(|entry| entry.fragment))
    }

    async fn load_metadata(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let Some(entry) = self.metadata_entry(hash).await? else {
            warn!("Failed to get metadata for fragment, no item found");

            return Err(StoreError::from(AddressNotFound::from(
                Address::zero_context_hash(hash),
            )));
        };

        entry.fragment.ok_or_else(|| {
            warn!("No fragment found on metadata from store: {entry:?}");
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
            Some(r) => r,
            None => s3_fut.await,
        };

        // Metadata resolved, so the store believes this payload is durable, but S3 does not have
        // it. Reads of this hash will fail from every partition referencing it, and because a
        // missing object is reported as a plain not-found, it is otherwise indistinguishable
        // from content that was never stored. Say so explicitly and count it.
        let s3_contents = s3_contents.inspect_err(|error| {
            if error.is_address_not_found() {
                self.missing_payload_counter
                    .add(1, &self.labels_missing_payload);
                error!(
                    %hash,
                    "Payload is published in metadata but absent from S3; content for this hash \
                     has been lost and will not be repaired by writing it again"
                );
            }
        })?;

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
        timed!(self.latency_histogram, &self.labels_put, {
            self.put_deduplicated(repository, address, fragment, payload)
                .await
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

            let original_metadata = self.metadata_with_size_validation(address.hash).await?;

            info!(?original_metadata, "Loaded metadata");

            if original_metadata.flags & FragmentFlags::PayloadObliterated != 0 {
                info!("Fragment has already been obliterated");
                return Ok(());
            }

            // Another obliteration owns the mark. Removing this partition's reference is what
            // this call has to do and is safe whoever owns the mark, since it is a single row
            // keyed by this partition and context. The payload and the metadata are the mark
            // owner's to decide: it counts the references after its own drain and will see this
            // one gone, whereas racing it here could delete a payload it has decided to keep.
            if original_metadata.flags & FragmentFlags::PayloadObliterating != 0 {
                info!("Another obliteration holds the mark; removing this association only");

                self.delete_association(repository, address).await?;
                stats
                    .num_fragments
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                return Ok(());
            }

            // Removing this partition's reference is the part that has to happen. Everything after
            // it — deleting the shared payload, marking the metadata — is cleanup that only
            // applies once nothing references the content any more.
            // Mark before touching the association. A put that reads the mark backs off, which
            // is what stops a reference appearing while this runs — including the reference this
            // obliteration is about to delete being written straight back by the very partition
            // it is being deleted for.
            let considered_metadata = {
                let mut considered = original_metadata;
                considered.flags |= FragmentFlags::PayloadObliterating;

                self.update_metadata(address, considered, original_metadata)
                    .await?;
                info!(?considered, "Marked for obliteration");
                considered
            };

            self.delete_association(repository, address).await?;
            stats
                .num_fragments
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // A put that read the row before the mark went on may still be about to write its
            // association. Association reads are strongly consistent, so this is not waiting for
            // consistency: it is waiting for those writes to land, so the count below sees them.
            // Afterwards no further reference can appear, which is what lets a single count
            // decide, and lets an association be written plainly rather than in a transaction.
            tokio::time::sleep(Duration::from_millis(self.obliteration_drain_millis)).await;

            info!("Association deleted, counting remaining associations...");
            if self.has_associations(address.hash).await? {
                info!("Fragment still associated, clearing the mark");
                return self
                    .update_metadata(address, original_metadata, considered_metadata)
                    .await
                    .inspect_err(|e| {
                        warn!("Failed to reset metadata back to original state: {e:?}");
                    });
            }

            // Only now is the content definitely going away, so its sub-fragments can go with it.
            if considered_metadata.flags & FragmentFlags::PayloadFragmented != 0 {
                info!("Fragment is fragmented");
                // There's no reason we couldn't use the `considered_metadata` here, since
                // `read_payload` only cares about the size fields (which haven't changed), but it
                // feels wrong given it doesn't explicitly match the metadata for what's currently
                // in S3.
                let payload = self
                    .read_payload(
                        self.get_s3_object_contents(address.hash).await?,
                        address.hash,
                        original_metadata,
                    )?
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
                    return Err(StoreError::internal(format!(
                        "Failed to obliterate immutable {address}"
                    )));
                }

                info!("Done obliterating sub-fragments");
            }

            self.delete_payload(address.hash).await?;

            stats
                .num_payloads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let mut obliterated_metadata = considered_metadata;
            obliterated_metadata.flags = FragmentFlags::PayloadObliterated.bits();
            obliterated_metadata.size_payload = 0;
            obliterated_metadata.size_content = 0;

            // Leave a tombstone rather than removing the row: it keeps a repeat obliteration
            // idempotent, and is what a policy refusing re-upload would key on.
            self.update_metadata(address, obliterated_metadata, considered_metadata)
                .await
                .inspect_err(|e| {
                    // At this point we've already deleted the underlying payload, so there's not any
                    // point in trying to revert the metadata, that fragment is just well and truly
                    // broken.
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
    use std::sync::atomic::AtomicUsize;
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
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::operation::get_object::GetObjectOutput;
    use aws_sdk_s3::operation::head_object::HeadObjectOutput;
    use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsError;
    use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsOutput;
    use aws_sdk_s3::operation::put_object::PutObjectError;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::primitives::SdkBody;
    use aws_sdk_s3::types::ObjectVersion;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::client::result::SdkError;
    use aws_smithy_runtime_api::client::result::ServiceError;
    use aws_smithy_runtime_api::client::result::TimeoutError;
    use aws_smithy_types::DateTime;
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
    const ABANDONED_ETAG: &str = "\"abandoned-object\"";

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

    /// Mock an association write. Obliteration marks the metadata row before removing a payload
    /// and counts references afterwards, so a put that saw no mark is either counted by that pass
    /// or has already backed off, and nothing about this write is conditional.
    fn mock_associate_fragment(dynamodb_mock: &mut MockDynamoDb, entry: &FragmentsEntry) {
        let item: HashMap<String, AttributeValue> = serde_dynamo::to_item(entry).unwrap();

        dynamodb_mock
            .expect_put_item()
            .withf(move |table, written| table.as_ref() == FRAGMENTS_TABLE_NAME && *written == item)
            .returning(|_, _| Ok(PutItemOutput::builder().build()));
    }

    /// Mock the metadata probe `put` issues, resolving to the supplied row (or to nothing).
    fn mock_metadata_probe(
        dynamodb_mock: &mut MockDynamoDb,
        hash: Hash,
        entry: Option<FragmentMetadataEntry>,
    ) {
        let key: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(hash)).unwrap();
        let item = entry.map(|e| serde_dynamo::to_item(e).unwrap());

        dynamodb_mock
            .expect_get_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(key), eq(true))
            .return_once(move |_, _, _| Ok(GetItemOutput::builder().set_item(item).build()));
    }

    /// Mock a fragments-table probe that answers every attempt with a miss. `put` re-probes on
    /// each attempt, and a `return_once` expectation would be re-matched rather than fall
    /// through.
    fn mock_fragments_probe_repeated(dynamodb_mock: &mut MockDynamoDb, entry: &FragmentsEntry) {
        let key: HashMap<String, AttributeValue> = serde_dynamo::to_item(entry).unwrap();

        dynamodb_mock
            .expect_get_item()
            .with(
                eq(Arc::<str>::from(FRAGMENTS_TABLE_NAME)),
                eq(key),
                eq(true),
            )
            .returning(|_, _, _| Ok(GetItemOutput::builder().set_item(None).build()));
    }

    /// Mock a metadata probe whose answer changes between reads, so a test can model the row
    /// moving underneath a writer. The last answer is repeated once the sequence is exhausted.
    fn mock_metadata_probe_sequence(
        dynamodb_mock: &mut MockDynamoDb,
        hash: Hash,
        answers: Vec<Option<FragmentMetadataEntry>>,
    ) {
        let key: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(hash)).unwrap();
        let items: Vec<Option<HashMap<String, AttributeValue>>> = answers
            .into_iter()
            .map(|entry| entry.map(|e| serde_dynamo::to_item(e).unwrap()))
            .collect();
        let reads = AtomicUsize::new(0);

        dynamodb_mock
            .expect_get_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(key), eq(true))
            .returning(move |_, _, _| {
                let index = reads.fetch_add(1, Ordering::Relaxed).min(items.len() - 1);

                Ok(GetItemOutput::builder()
                    .set_item(items[index].clone())
                    .build())
            });
    }

    /// Mock a metadata probe that answers every read, as the recovery poll issues several.
    fn mock_metadata_probe_repeated(
        dynamodb_mock: &mut MockDynamoDb,
        hash: Hash,
        entry: Option<FragmentMetadataEntry>,
    ) {
        let key: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(hash)).unwrap();
        let item = entry.map(|e| serde_dynamo::to_item(e).unwrap());

        dynamodb_mock
            .expect_get_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(key), eq(true))
            .returning(move |_, _, _| Ok(GetItemOutput::builder().set_item(item.clone()).build()));
    }

    /// Mock the conditional publish that follows an upload, asserting it writes exactly
    /// `fragment` and nothing more.
    fn mock_publish_metadata(dynamodb_mock: &mut MockDynamoDb, hash: Hash, fragment: Fragment) {
        let published: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(hash).with_fragment(fragment))
                .unwrap();

        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(1)
            .withf(move |table, item, _| {
                table.as_ref() == METADATA_TABLE_NAME && *item == published
            })
            .returning(|_, _, _| Ok(PutItemOutput::builder().build()));
    }

    async fn initialize_immutable_store_with_grace(
        s3: S3,
        dynamodb: DynamoDb,
        grace_millis: u64,
    ) -> AwsImmutableStore {
        let settings = AwsImmutableStoreSettings {
            s3: S3StoreSettings::new(BUCKET.to_string()),
            dynamodb: DynamoDbImmutableStoreSettings::new(
                FRAGMENTS_TABLE_NAME.to_string(),
                METADATA_TABLE_NAME.to_string(),
            ),
            force_write: false,
            batch_exist_submission_limit: 1000,
            abandoned_upload_grace_millis: Some(grace_millis),
            obliteration_drain_millis: Some(0),
        };

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                AwsImmutableStore::new(s3, dynamodb, &settings)
            })
            .await
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
            abandoned_upload_grace_millis: None,
            obliteration_drain_millis: Some(0),
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

        mock_metadata_probe(&mut dynamodb_mock, address.hash, None);
        mock_publish_metadata(&mut dynamodb_mock, address.hash, fragment);

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        s3mock
            .expect_put_object_if_absent()
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
                .expect_err("expected put to back off")
                .is_slow_down()
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

        mock_metadata_probe(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(obliterated_fragment)),
        );

        // A tombstone is not a deduplication source: the payload was deleted, so the bytes have
        // to be stored again and the row republished.
        mock_publish_metadata(&mut dynamodb_mock, address.hash, fragment);

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        s3mock
            .expect_put_object_if_absent()
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

        // Nothing is stored for this hash, so there is nothing to deduplicate against and the
        // caller has to supply the bytes.
        mock_metadata_probe(&mut dynamodb_mock, address.hash, None);

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

    /// `force_write` is an operator override, but it must still not tear an obliteration's lock
    /// off the row: doing so would resurrect content that is midway through being deleted.
    #[tokio::test]
    async fn test_put_immutable_force_write_respects_obliteration() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // Bare mocks: any upload or metadata write here is a test failure.
        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);
        let mut obliterating = fragment;
        obliterating.flags |= FragmentFlags::PayloadObliterating;

        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );
        mock_metadata_probe(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(obliterating)),
        );

        let settings = AwsImmutableStoreSettings {
            s3: S3StoreSettings::new(BUCKET.to_string()),
            dynamodb: DynamoDbImmutableStoreSettings::new(
                FRAGMENTS_TABLE_NAME.to_string(),
                METADATA_TABLE_NAME.to_string(),
            ),
            force_write: true,
            batch_exist_submission_limit: 1000,
            abandoned_upload_grace_millis: None,
            obliteration_drain_millis: Some(0),
        };

        let execution = setup_execution("test".to_string());
        let store = LORE_CONTEXT
            .scope(execution, async move {
                AwsImmutableStore::new(s3mock, dynamodb_mock, &settings)
            })
            .await;

        assert!(
            Arc::new(store)
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .expect_err("force_write must not override an obliteration")
                .is_slow_down()
        );
    }

    /// Build a `force_write` store, the operator override that replaces whatever is stored.
    async fn initialize_force_write_store(s3: S3, dynamodb: DynamoDb) -> AwsImmutableStore {
        let settings = AwsImmutableStoreSettings {
            s3: S3StoreSettings::new(BUCKET.to_string()),
            dynamodb: DynamoDbImmutableStoreSettings::new(
                FRAGMENTS_TABLE_NAME.to_string(),
                METADATA_TABLE_NAME.to_string(),
            ),
            force_write: true,
            batch_exist_submission_limit: 1000,
            abandoned_upload_grace_millis: Some(100),
            obliteration_drain_millis: Some(0),
        };

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                AwsImmutableStore::new(s3, dynamodb, &settings)
            })
            .await
    }

    fn conditional_check_failed() -> AwsError<SdkError<PutItemError, HttpResponse>> {
        aws_error(
            PutItemError::ConditionalCheckFailedException(
                ConditionalCheckFailedException::builder().build(),
            ),
            400u16,
        )
    }

    fn precondition_failed() -> AwsError<SdkError<PutObjectError, HttpResponse>> {
        aws_error(
            PutObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build()),
            412u16,
        )
    }

    /// A publish rejected because the row moved is not a reason to store the payload again: the
    /// bytes are already there, so only the metadata write is retried, against what is now stored.
    #[tokio::test]
    async fn test_put_immutable_publish_reconditions_without_reuploading() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);
        let mut moved = fragment;
        moved.flags |= FragmentFlags::PayloadStoredDurable;

        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );
        // Probed as `fragment`, but by publish time the row reads as `moved`.
        mock_metadata_probe_sequence(
            &mut dynamodb_mock,
            address.hash,
            vec![
                Some(FragmentMetadataEntry::new(address.hash).with_fragment(fragment)),
                Some(FragmentMetadataEntry::new(address.hash).with_fragment(moved)),
            ],
        );

        // Exactly one upload, however many times the publish is retried.
        s3mock
            .expect_put_object::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _| Ok(PutObjectOutput::builder().build()));

        let mut seq = mockall::Sequence::default();
        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(1)
            .in_sequence(&mut seq)
            .withf(move |_, _, condition| *condition == MetadataWriteCondition::Unchanged(fragment))
            .returning(|_, _, _| Err(conditional_check_failed()));
        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(1)
            .in_sequence(&mut seq)
            .withf(move |_, _, condition| *condition == MetadataWriteCondition::Unchanged(moved))
            .returning(|_, _, _| Ok(PutItemOutput::builder().build()));

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let store = initialize_force_write_store(s3mock, dynamodb_mock).await;

        Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("the publish should have re-conditioned and succeeded");
    }

    /// A `force_write` that replaced the stored bytes but never managed to publish has left them
    /// described by someone else's metadata, and nothing will repair that. It must surface as a
    /// failure an operator can see, not as a retry hint.
    #[tokio::test]
    async fn test_put_immutable_force_write_publish_exhaustion_is_loud() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );
        // The row never settles.
        mock_metadata_probe_repeated(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(fragment)),
        );

        s3mock
            .expect_put_object::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _| Ok(PutObjectOutput::builder().build()));

        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(PUBLISH_MAX_ATTEMPTS)
            .returning(|_, _, _| Err(conditional_check_failed()));

        let store = initialize_force_write_store(s3mock, dynamodb_mock).await;

        let error = Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect_err("an unrepairable force_write must not look like a retry");

        assert!(
            error.is_internal() && !error.is_slow_down(),
            "expected a visible failure, got {error:?}"
        );
    }

    /// An object left under a key whose content has been obliterated outlived that content: a
    /// tombstone is a row, but it does not account for an object. It must be discarded rather
    /// than mistaken for a published payload, which would wedge the hash forever and leave
    /// deleted bytes in S3.
    #[tokio::test]
    async fn test_put_immutable_discards_object_left_by_obliteration() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);
        let tombstone = Fragment {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 0,
            size_content: 0,
        };

        // Two attempts: the first discards the remnant, the second stores the content.
        mock_fragments_probe_repeated(&mut dynamodb_mock, &entry);
        mock_metadata_probe_repeated(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(tombstone)),
        );

        let mut seq = mockall::Sequence::default();

        // The remnant blocks the first upload...
        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _| Err(precondition_failed()));

        // ...so it is deleted...
        s3mock
            .expect_list_versions()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(ListObjectVersionsOutput::builder().build()));
        s3mock
            .expect_delete_object()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _| Ok(DeleteObjectOutput::builder().build()));

        // ...and the retry stores this writer's bytes in its place.
        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _| Ok(PutObjectOutput::builder().build()));

        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(1)
            .withf(move |_, _, condition| {
                *condition == MetadataWriteCondition::Unchanged(tombstone)
            })
            .returning(|_, _, _| Ok(PutItemOutput::builder().build()));

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let store = initialize_immutable_store_with_grace(s3mock, dynamodb_mock, 100).await;

        Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("should have discarded the remnant and stored the content");
    }

    /// Reclaiming stores this writer's bytes, so losing the publish to an obliteration must
    /// withdraw them. Otherwise content an obliteration is deleting comes back into S3 through
    /// the reclaim path — the same hole the tombstone handling closes on the upload path.
    #[tokio::test]
    async fn test_put_immutable_discards_reclaimed_bytes_when_obliteration_wins() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);
        let mut obliterating = fragment;
        obliterating.flags |= FragmentFlags::PayloadObliterating;

        mock_fragments_probe_repeated(&mut dynamodb_mock, &entry);
        mock_metadata_probe_sequence(
            &mut dynamodb_mock,
            address.hash,
            vec![
                // Nothing stored when the put probes, nor when the object is resolved...
                None,
                None,
                // ...but an obliteration owns the hash by the time it is published.
                Some(FragmentMetadataEntry::new(address.hash).with_fragment(obliterating)),
            ],
        );

        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _| Err(precondition_failed()));
        s3mock.expect_head_object().times(1).returning(|_, _| {
            Ok(HeadObjectOutput::builder()
                .e_tag(ABANDONED_ETAG)
                .last_modified(DateTime::from_secs(1))
                .build())
        });
        s3mock
            .expect_put_object_if_match::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _, _| Ok(PutObjectOutput::builder().build()));

        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(1)
            .returning(|_, _, _| Err(conditional_check_failed()));

        // The reclaimed bytes must be withdrawn.
        s3mock
            .expect_list_versions()
            .times(1)
            .returning(|_, _| Ok(ListObjectVersionsOutput::builder().build()));
        s3mock
            .expect_delete_object()
            .times(1)
            .returning(|_, _, _| Ok(DeleteObjectOutput::builder().build()));

        let store = initialize_immutable_store_with_grace(s3mock, dynamodb_mock, 100).await;

        assert!(
            Arc::new(store)
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .expect_err("an obliterated hash must not accept a put")
                .is_slow_down()
        );
    }

    /// `is_abandoned` is the only gate on reclaiming another writer's object, and the only
    /// clock-dependent logic here, so its edges are worth pinning directly.
    #[test]
    fn abandonment_is_decided_conservatively() {
        let now = now_millis();
        let recent = DateTime::from_millis((now - 1_000) as i64);
        let old = DateTime::from_millis((now - 60_000) as i64);
        let future = DateTime::from_millis((now + 60_000) as i64);

        assert!(
            !is_abandoned(None, 10_000),
            "an object that cannot be dated must never be reclaimed"
        );
        assert!(
            !is_abandoned(Some(&future), 10_000),
            "an object dated in the future means clock skew, not abandonment"
        );
        assert!(
            !is_abandoned(Some(&recent), 10_000),
            "an object younger than the threshold belongs to a writer still finishing"
        );
        assert!(
            is_abandoned(Some(&old), 10_000),
            "an object older than the threshold has been left behind"
        );
    }

    /// Sub-second thresholds cannot mean what they say if the stored time is truncated to whole
    /// seconds, and the truncation error is up to a second — larger than the smallest threshold
    /// the settings allow.
    #[test]
    fn stored_time_keeps_sub_second_precision() {
        assert_eq!(datetime_millis(&DateTime::from_millis(1_500)), Some(1_500));
        assert_eq!(datetime_millis(&DateTime::from_millis(999)), Some(999));
        assert_eq!(
            datetime_millis(&DateTime::from_secs(-1)),
            None,
            "a time before the epoch is not a usable age"
        );
    }

    /// A payload that is published but missing from S3 is otherwise indistinguishable from
    /// content that was never stored, because a missing object surfaces as a plain not-found. It
    /// has to be called out, since nothing repairs it and every partition referencing the hash is
    /// affected.
    #[tokio::test]
    #[traced_test]
    async fn test_get_immutable_reports_published_metadata_with_no_object() {
        let repository = random::<Context>();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        let entry = FragmentsEntry::new(repository, address);
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );

        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(address.hash.to_string()), eq(None))
            .return_once(move |_, _, _| {
                Err(aws_error(
                    GetObjectError::NoSuchKey(NoSuchKey::builder().build()),
                    404u16,
                ))
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let error = Arc::new(store)
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect_err("a published payload with no object cannot be read");

        assert!(error.is_address_not_found());
        assert!(
            logs_contain("published in metadata but absent from S3"),
            "losing a payload underneath the store must not look like an ordinary miss"
        );

        let _ = fragment;
    }

    /// The metadata table holds rows written long before any of this, and this design keeps its
    /// shape untouched so no table change or backfill is needed. Guards against a future addition
    /// quietly changing what a published row looks like.
    #[test]
    fn published_rows_keep_their_original_shape() {
        let (fragment, address, _) = fragment::generate_random();

        let published: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(address.hash).with_fragment(fragment))
                .unwrap();

        let mut attributes = published.keys().cloned().collect::<Vec<_>>();
        attributes.sort();
        assert_eq!(
            attributes,
            vec!["flags", "hash", "size_content", "size_payload"],
            "a published row must carry exactly the attributes existing rows already have"
        );

        let parsed: FragmentMetadataEntry = serde_dynamo::from_item(published).unwrap();
        assert_eq!(parsed.fragment, Some(fragment));
    }

    /// A row that is only marked for obliteration still has its payload — it is removed after the
    /// references are counted, and the mark is cleared again if any remain. Refusing reads there
    /// would hide content from every partition holding it, and hide it permanently if the
    /// obliteration that set the mark never finished.
    #[tokio::test]
    async fn test_get_immutable_marked_for_obliteration_is_still_readable() {
        let repository = random::<Context>();
        let (mut fragment, address, payload) = fragment::generate_random();
        fragment.flags |= FragmentFlags::PayloadObliterating;

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );

        let key: HashMap<String, AttributeValue> =
            serde_dynamo::to_item(FragmentMetadataEntry::new(address.hash)).unwrap();
        let row =
            serde_dynamo::to_item(FragmentMetadataEntry::new(address.hash).with_fragment(fragment))
                .unwrap();
        dynamodb_mock
            .expect_get_item()
            .with(eq(Arc::<str>::from(METADATA_TABLE_NAME)), eq(key), eq(true))
            .return_once(move |_, _, _| Ok(GetItemOutput::builder().set_item(Some(row)).build()));

        let stored = payload.to_vec();
        s3mock
            .expect_get_object()
            .with(eq(BUCKET), eq(address.hash.to_string()), eq(None))
            .return_once(move |_, _, _| {
                Ok(GetObjectOutput::builder()
                    .set_body(Some(stored.into()))
                    .build())
            });

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let (read_fragment, read_payload) = Arc::new(store)
            .get(repository.into(), address, StoreMatch::MatchFull)
            .await
            .expect("a marked payload is still stored and must still be readable");

        assert_eq!(read_fragment, fragment);
        assert_eq!(read_payload, payload);
    }

    /// Deduplication skips the upload, not the requirement to present the bytes. A caller that
    /// does not already reference the content cannot attach it by naming its hash alone.
    #[tokio::test]
    async fn test_put_immutable_payload_required_to_deduplicate() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // This partition holds no reference to the content...
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        // ...and although the payload is durable elsewhere, that alone does not entitle this
        // caller to a reference to it. No association write is mocked: making one is a failure.
        mock_metadata_probe(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(fragment)),
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        assert!(
            Arc::new(store)
                .put(repository.into(), address, fragment, None, false)
                .await
                .expect_err("a hash alone should not attach content to a partition")
                .is_internal()
        );
    }

    /// An object left behind by a writer that died before publishing must not wedge the hash.
    /// After waiting out the grace period, the key is taken over with bytes this writer can
    /// vouch for — rather than trying to work out what the abandoned ones were.
    #[tokio::test]
    async fn test_put_immutable_reclaims_abandoned_object() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        mock_fragments_probe_repeated(&mut dynamodb_mock, &entry);
        // Nothing is ever published by the writer that left the object behind.
        mock_metadata_probe_repeated(&mut dynamodb_mock, address.hash, None);

        // The conditional upload refuses to replace the abandoned object.
        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _| Err(precondition_failed()));

        // It is identified, then replaced only if it is still that same object.
        s3mock.expect_head_object().times(1).returning(|_, _| {
            Ok(HeadObjectOutput::builder()
                .e_tag(ABANDONED_ETAG)
                .last_modified(DateTime::from_secs(1))
                .build())
        });
        s3mock
            .expect_put_object_if_match::<Vec<u8>>()
            .times(1)
            .withf(|_, _, _, etag| etag == ABANDONED_ETAG)
            .returning(|_, _, _, _| Ok(PutObjectOutput::builder().build()));

        // This writer's own fragment is published, because these are now its own bytes.
        mock_publish_metadata(&mut dynamodb_mock, address.hash, fragment);
        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let store = initialize_immutable_store_with_grace(s3mock, dynamodb_mock, 100).await;

        Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("should have reclaimed the abandoned object");
    }

    /// An object stored moments ago belongs to a writer that is most likely still finishing.
    /// Its representation is the one that will win, so this writer must leave it alone and hand
    /// the wait back to the caller rather than reclaiming it or holding the request.
    #[tokio::test]
    async fn test_put_immutable_backs_off_from_an_unpublished_upload() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        mock_fragments_probe_repeated(&mut dynamodb_mock, &entry);
        mock_metadata_probe_repeated(&mut dynamodb_mock, address.hash, None);

        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _| Err(precondition_failed()));

        // Stored just now. No reclaim is mocked: taking it over would be a test failure.
        let stored_at = DateTime::from_millis(now_millis() as i64);
        s3mock.expect_head_object().times(1).returning(move |_, _| {
            Ok(HeadObjectOutput::builder()
                .e_tag(ABANDONED_ETAG)
                .last_modified(stored_at)
                .build())
        });

        let store = initialize_immutable_store_with_grace(s3mock, dynamodb_mock, 60_000).await;

        let error = Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect_err("should have backed off rather than reclaiming a live upload");

        assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
    }

    /// Two writers can reach recovery for the same abandoned object. Conditioning the reclaim on
    /// the object being unchanged makes it single-winner: the loser must publish nothing, or it
    /// would describe the winner's bytes with its own fragment.
    #[tokio::test]
    async fn test_put_immutable_losing_the_reclaim_publishes_nothing() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        mock_fragments_probe_repeated(&mut dynamodb_mock, &entry);
        mock_metadata_probe_repeated(&mut dynamodb_mock, address.hash, None);

        // No publish is mocked: writing metadata after losing the reclaim would describe another
        // writer's bytes, so any conditional put here is a test failure.
        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(PUT_MAX_ATTEMPTS)
            .returning(|_, _, _| Err(precondition_failed()));
        s3mock
            .expect_head_object()
            .times(PUT_MAX_ATTEMPTS)
            .returning(|_, _| {
                Ok(HeadObjectOutput::builder()
                    .e_tag(ABANDONED_ETAG)
                    .last_modified(DateTime::from_secs(1))
                    .build())
            });

        // Another writer reclaimed it first every time, so the entity tag never matches.
        s3mock
            .expect_put_object_if_match::<Vec<u8>>()
            .times(PUT_MAX_ATTEMPTS)
            .returning(|_, _, _, _| Err(precondition_failed()));

        let store = initialize_immutable_store_with_grace(s3mock, dynamodb_mock, 100).await;

        let error = Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect_err("losing every reclaim should not report success");

        assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
    }

    /// A writer whose upload was conditional cannot know its bytes are still the stored ones —
    /// they may have been reclaimed while it was stalled. So a rejected publish must make it
    /// stand down and re-probe, never overwrite the other writer's metadata with its own
    /// fragment.
    #[tokio::test]
    async fn test_put_immutable_conditional_writer_does_not_publish_over_a_reclaimer() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let mut s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);
        let reclaimer = Fragment {
            flags: FragmentFlags::PayloadCompressedZstd.bits(),
            size_payload: fragment.size_payload / 2,
            size_content: fragment.size_content,
        };

        mock_fragments_probe_repeated(&mut dynamodb_mock, &entry);
        // Nothing stored at probe time; by publish time a reclaimer owns the hash.
        mock_metadata_probe_sequence(
            &mut dynamodb_mock,
            address.hash,
            vec![
                None,
                Some(FragmentMetadataEntry::new(address.hash).with_fragment(reclaimer)),
            ],
        );

        s3mock
            .expect_put_object_if_absent::<Vec<u8>>()
            .times(1)
            .returning(|_, _, _| Ok(PutObjectOutput::builder().build()));

        // Exactly one publish attempt. A second would be this writer overwriting the reclaimer's
        // metadata with a fragment describing bytes that are no longer stored.
        dynamodb_mock
            .expect_put_item_conditional::<MetadataWriteCondition>()
            .times(1)
            .withf(|_, _, condition| *condition == MetadataWriteCondition::Absent)
            .returning(|_, _, _| Err(conditional_check_failed()));

        // Standing down leads to deduplicating against what the reclaimer published.
        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("should have stood down and deduplicated");
    }

    /// The payload is already durable under a different partition, so this put records a
    /// reference to it instead of uploading anything. No S3 call and no metadata write: the
    /// stored representation is adopted as-is.
    #[tokio::test]
    async fn test_put_immutable_deduplicates_across_partitions() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // A bare S3 mock with no expectations: any upload here is a test failure.
        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        // This partition has no association yet...
        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry.clone(),
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
        );

        // ...but the payload is already committed, so it is durable in S3.
        mock_metadata_probe(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(fragment)),
        );

        mock_associate_fragment(&mut dynamodb_mock, &entry);

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("failed to deduplicate against durable content");
    }

    /// A put whose exact association already exists against committed content writes nothing at
    /// all: no S3, no metadata, not even an association.
    #[tokio::test]
    async fn test_put_immutable_full_match_writes_nothing() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let entry = FragmentsEntry::new(repository, address);

        mock_lookup_fragments(
            &mut dynamodb_mock,
            entry,
            StoreMatch::MatchFull,
            StoreMatch::MatchFull,
        );
        mock_metadata_probe(
            &mut dynamodb_mock,
            address.hash,
            Some(FragmentMetadataEntry::new(address.hash).with_fragment(fragment)),
        );

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        Arc::new(store)
            .put(repository.into(), address, fragment, Some(payload), false)
            .await
            .expect("failed to put an already stored fragment");
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

        mock_metadata_probe(&mut dynamodb_mock, address.hash, None);
        mock_publish_metadata(&mut dynamodb_mock, address.hash, fragment);

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
            .expect_put_object_if_absent()
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
            flags: FragmentFlags::PayloadObliterated.bits(),
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

        // Another obliteration owns the mark, so the payload and the metadata are its to decide.
        // Removing this partition's own reference is still this call's job, and neither S3 nor a
        // metadata write is mocked: touching either here would be a test failure.
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

        assert_eq!(
            stats.num_fragments.load(Ordering::Relaxed),
            1,
            "the reference this call was asked to remove must still be removed"
        );
        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
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

    /// The reference being obliterated must not be removable while a put could write it straight
    /// back. Marking the row first is what prevents that: a put reading the mark backs off, so
    /// the partition the content is being deleted for cannot restore its own reference and leave
    /// the obliteration reporting success over content that is still referenced.
    ///
    /// Pins the ordering rather than the race, since the race is what the ordering rules out.
    #[tokio::test]
    async fn test_obliterate_marks_before_removing_the_association() {
        let repository = random::<Context>();

        let s3mock = MockS3Impl::default();
        let mut dynamodb_mock = MockDynamoDb::default();

        let (fragment, address) =
            mock_load_fragment_metadata(&mut dynamodb_mock, None, false /* fail */);

        let mut marked = fragment;
        marked.flags |= FragmentFlags::PayloadObliterating;
        let mut order = mockall::Sequence::default();

        dynamodb_mock
            .expect_put_item_conditional::<UpdateMetadataCondition>()
            .times(1)
            .in_sequence(&mut order)
            .withf(move |table, _, condition| {
                table.as_ref() == METADATA_TABLE_NAME
                    && *condition == UpdateMetadataCondition(fragment)
            })
            .returning(|_, _, _| Ok(PutItemOutput::builder().build()));

        dynamodb_mock
            .expect_delete_item()
            .times(1)
            .in_sequence(&mut order)
            .withf(move |table, _| table.as_ref() == FRAGMENTS_TABLE_NAME)
            .returning(|_, _| Ok(DeleteItemOutput::builder().build()));

        // A reference remains, so the mark is cleared and nothing else happens. Reaching this at
        // all means the mark was already in place when the association was removed.
        mock_count_associations(&mut dynamodb_mock, address.hash, 1, false /* fail */);

        dynamodb_mock
            .expect_put_item_conditional::<UpdateMetadataCondition>()
            .times(1)
            .in_sequence(&mut order)
            .withf(move |_, _, condition| *condition == UpdateMetadataCondition(marked))
            .returning(|_, _, _| Ok(PutItemOutput::builder().build()));

        let store = initialize_immutable_store(s3mock, dynamodb_mock).await;

        let stats: Arc<StoreObliterateStats> = Default::default();
        Arc::new(store)
            .obliterate(repository.into(), address, stats.clone())
            .await
            .expect("obliterate should leave a still-referenced payload alone");

        assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
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

        // The parent's own association is now removed before its sub-fragments are visited, so
        // that they are only obliterated once the parent is known to be going away.
        mock_remove_association(
            &mut dynamodb_mock,
            repository,
            address,
            false, /* fail */
        );
        mock_count_associations(&mut dynamodb_mock, address.hash, 0, false /* fail */);

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
            // Associations for both sub-fragments and the parent
            (SUB_FRAGMENT_COUNT + 1) as usize
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

    /// Concurrency tests that drive the real store against an in-memory stand-in for `DynamoDB`
    /// and S3.
    ///
    /// The fake implements the parts of the semantics the protocol actually leans on: per-object
    /// and per-item atomicity, conditional puts, and write-once objects. That is what lets these
    /// tests answer the question they exist for — whether concurrent writers holding the same
    /// content in *different* representations can leave the stored blob and the metadata
    /// describing it disagreeing, which would make every later read of that hash fail.
    mod concurrency {
        use std::collections::HashSet;
        use std::sync::Mutex;

        use super::*;

        /// Number of writers racing for the same hash.
        const WRITERS: u8 = 6;
        /// Base compressed length; each writer's representation is a different length.
        const BASE_PAYLOAD_LEN: usize = 64;
        /// Uncompressed size, shared by every representation because it is the same content.
        const CONTENT_SIZE: u64 = 4096;

        /// The stored state for a single hash, which is all these races need.
        ///
        /// Guarded by one mutex, so each operation is atomic with respect to the others — the
        /// property S3 gives per object and `DynamoDB` per item, and the only one these tests
        /// need, since the corruption they cover comes from two separately atomic writes to two
        /// independent stores rather than from a torn individual write.
        #[derive(Default)]
        struct FakeState {
            metadata: Option<HashMap<String, AttributeValue>>,
            object: Option<Vec<u8>>,
            associations: HashSet<String>,
            uploads: usize,
            /// Bumped on every write, so the entity tag changes exactly when the object does.
            generation: u64,
            stored_at_millis: u64,
            /// When set, the next metadata write fails, standing in for the throttling that
            /// corrupts the store before this change.
            fail_next_metadata_write: bool,
            /// When set, conditional metadata writes always succeed and uploads overwrite freely,
            /// modelling the unguarded write this store used before S3 arbitrated between
            /// writers. Used by the negative control to show what the conditions actually buy.
            unprotected: bool,
        }

        impl FakeState {
            fn etag(&self) -> String {
                format!("\"generation-{}\"", self.generation)
            }

            fn store(&mut self, body: Vec<u8>) {
                self.object = Some(body);
                self.generation += 1;
                self.stored_at_millis = now_millis();
                self.uploads += 1;
            }

            /// Evaluate a publish condition against the current row.
            fn permits(&self, condition: &MetadataWriteCondition) -> bool {
                if self.unprotected {
                    return true;
                }

                match condition {
                    MetadataWriteCondition::Absent => self.metadata.is_none(),
                    MetadataWriteCondition::Unchanged(expected) => {
                        self.metadata.as_ref().is_some_and(|row| {
                            row.get("flags") == Some(&AttributeValue::N(expected.flags.to_string()))
                                && row.get("size_payload")
                                    == Some(&AttributeValue::N(expected.size_payload.to_string()))
                                && row.get("size_content")
                                    == Some(&AttributeValue::N(expected.size_content.to_string()))
                        })
                    }
                }
            }
        }

        /// Identity of an association, taken from the sort key of a fragments row.
        fn association_key(item: &HashMap<String, AttributeValue>) -> String {
            format!("{:?}", item.get(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE))
        }

        /// Writer `index`'s representation of the shared content: a distinct codec, a distinct
        /// compressed length, and bytes that identify which writer produced them.
        fn representation(index: u8) -> (Fragment, Bytes) {
            let len = BASE_PAYLOAD_LEN + usize::from(index);
            let codec = match index % 3 {
                0 => FragmentFlags::PayloadCompressedLZ4,
                1 => FragmentFlags::PayloadCompressedZstd,
                _ => FragmentFlags::PayloadCompressedOodle2,
            };

            let fragment = Fragment {
                flags: codec.bits(),
                size_payload: len as u32,
                size_content: CONTENT_SIZE,
            };

            (fragment, Bytes::from(vec![index; len]))
        }

        /// Build a store whose `DynamoDB` and S3 calls are served by `state`.
        async fn fake_store(state: Arc<Mutex<FakeState>>) -> Arc<AwsImmutableStore> {
            let mut dynamodb = MockDynamoDb::default();
            let mut s3 = MockS3Impl::default();

            let get_state = state.clone();
            dynamodb
                .expect_get_item()
                .returning(move |table, key, _consistent| {
                    let state = get_state.lock().unwrap();
                    let item = if table.as_ref() == METADATA_TABLE_NAME {
                        state.metadata.clone()
                    } else if state.associations.contains(&association_key(&key)) {
                        Some(key)
                    } else {
                        None
                    };

                    Ok(GetItemOutput::builder().set_item(item).build())
                });

            let put_state = state.clone();
            dynamodb.expect_put_item().returning(move |table, item| {
                assert_eq!(
                    table.as_ref(),
                    FRAGMENTS_TABLE_NAME,
                    "metadata must only ever be written conditionally"
                );
                put_state
                    .lock()
                    .unwrap()
                    .associations
                    .insert(association_key(&item));

                Ok(PutItemOutput::builder().build())
            });

            let conditional_state = state.clone();
            dynamodb
                .expect_put_item_conditional::<MetadataWriteCondition>()
                .returning(move |table, item, condition| {
                    assert_eq!(table.as_ref(), METADATA_TABLE_NAME);
                    let mut state = conditional_state.lock().unwrap();

                    if state.fail_next_metadata_write {
                        state.fail_next_metadata_write = false;

                        return Err(aws_error(
                            PutItemError::ProvisionedThroughputExceededException(
                                ProvisionedThroughputExceededException::builder().build(),
                            ),
                            400u16,
                        ));
                    }

                    if !state.permits(&condition) {
                        return Err(aws_error(
                            PutItemError::ConditionalCheckFailedException(
                                ConditionalCheckFailedException::builder()
                                    .set_item(state.metadata.clone())
                                    .build(),
                            ),
                            400u16,
                        ));
                    }

                    state.metadata = Some(item);

                    Ok(PutItemOutput::builder().build())
                });

            let upload_state = state.clone();
            s3.expect_put_object_if_absent::<Vec<u8>>()
                .returning(move |_bucket, _key, body| {
                    let mut state = upload_state.lock().unwrap();

                    // Write-once, as S3 enforces for a conditional put: an existing object is
                    // never silently replaced.
                    if state.object.is_some() && !state.unprotected {
                        return Err(aws_error(
                            PutObjectError::generic(
                                ErrorMetadata::builder().code("PreconditionFailed").build(),
                            ),
                            412u16,
                        ));
                    }

                    state.store(body);

                    Ok(PutObjectOutput::builder().build())
                });

            let read_state = state.clone();
            s3.expect_get_object()
                .returning(move |_bucket, _key, _range| {
                    let object = read_state.lock().unwrap().object.clone();

                    Ok(GetObjectOutput::builder()
                        .set_body(object.map(Into::into))
                        .build())
                });

            let head_state = state.clone();
            s3.expect_head_object().returning(move |_bucket, _key| {
                let state = head_state.lock().unwrap();

                Ok(HeadObjectOutput::builder()
                    .e_tag(state.etag())
                    .last_modified(DateTime::from_millis(state.stored_at_millis as i64))
                    .build())
            });

            // Reclaiming succeeds only while the object is still the one that was inspected,
            // which is what makes it single-winner.
            let reclaim_state = state.clone();
            s3.expect_put_object_if_match::<Vec<u8>>().returning(
                move |_bucket, _key, body, etag| {
                    let mut state = reclaim_state.lock().unwrap();

                    if state.etag() != etag {
                        return Err(aws_error(
                            PutObjectError::generic(
                                ErrorMetadata::builder().code("PreconditionFailed").build(),
                            ),
                            412u16,
                        ));
                    }

                    state.store(body);

                    Ok(PutObjectOutput::builder().build())
                },
            );

            let delete_state = state.clone();
            s3.expect_delete_object()
                .returning(move |_bucket, _key, _version| {
                    delete_state.lock().unwrap().object = None;

                    Ok(DeleteObjectOutput::builder().build())
                });

            Arc::new(initialize_immutable_store(s3, dynamodb).await)
        }

        /// Describe how the stored blob and the published metadata disagree, or `None` when they
        /// are a matched pair.
        ///
        /// This is the invariant everything here exists to protect: whatever metadata is
        /// published must describe exactly the bytes sitting in S3, because a reader fetches the
        /// two independently and fails if they do not line up.
        fn cohesion_violation(state: &FakeState) -> Option<String> {
            let row = state.metadata.as_ref()?;
            let entry: FragmentMetadataEntry = serde_dynamo::from_item(row.clone()).unwrap();

            let published = entry.fragment?;

            let Some(object) = state.object.as_ref() else {
                return Some("metadata was published with no blob in S3".to_string());
            };

            if published.size_payload as usize != object.len() {
                return Some(format!(
                    "published size_payload {} does not match the stored blob length {}",
                    published.size_payload,
                    object.len()
                ));
            }

            // Every writer fills its payload with its own index, so the blob names the writer
            // whose bytes actually survived. The published metadata must be that writer's.
            let writer = object[0];
            if !object.iter().all(|byte| *byte == writer) {
                return Some("the stored blob mixes bytes from more than one writer".to_string());
            }

            let (expected, _) = representation(writer);
            if published.flags != expected.flags {
                return Some(format!(
                    "writer {writer}'s blob was published with flags {:#x} instead of {:#x}",
                    published.flags, expected.flags
                ));
            }
            if published.size_content != expected.size_content {
                return Some("published size_content does not match the stored blob".to_string());
            }

            None
        }

        fn assert_blob_and_metadata_agree(state: &FakeState, context: &str) {
            if let Some(violation) = cohesion_violation(state) {
                panic!("{context}: {violation}");
            }
        }

        /// Many writers race to store the same content in different representations. Whatever
        /// ends up published has to describe the bytes that actually landed in S3.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn concurrent_writers_with_different_representations_stay_cohesive() {
            for iteration in 0..64 {
                let state = Arc::new(Mutex::new(FakeState::default()));
                let store = fake_store(state.clone()).await;
                let (_, shared, _) = fragment::generate_random();

                let mut writers = JoinSet::new();
                for index in 0..WRITERS {
                    let store = store.clone();
                    // Different partitions and contexts, same content: exactly the case
                    // deduplication is meant to collapse.
                    let partition = random::<Partition>();
                    let address = address_with_random_context(shared);
                    let (fragment, payload) = representation(index);

                    lore_base::lore_spawn!(writers, async move {
                        store
                            .put(partition, address, fragment, Some(payload), false)
                            .await
                    });
                }

                let mut stored = 0;
                while let Some(writer) = writers.join_next().await {
                    match writer.expect("writer panicked") {
                        Ok(()) => stored += 1,
                        // Backing off is a legitimate outcome: another writer's upload is in
                        // flight, and the caller retries rather than racing a second
                        // representation into the same object.
                        Err(error) => assert!(
                            error.is_slow_down(),
                            "iteration {iteration}: unexpected error {error:?}"
                        ),
                    }
                }

                let state = state.lock().unwrap();
                assert_blob_and_metadata_agree(&state, &format!("iteration {iteration}"));
                assert!(
                    stored >= 1,
                    "iteration {iteration}: every writer backed off, so none made progress"
                );
                assert_eq!(
                    state.uploads, 1,
                    "iteration {iteration}: the payload should be uploaded exactly once no \
                     matter how many writers raced for it"
                );
            }
        }

        /// Negative control for the race test above: the same writers, against a store whose
        /// metadata writes always succeed and whose objects can be overwritten — which is how
        /// this store behaved before S3 arbitrated between writers.
        ///
        /// With nothing serialising them, writers that probed before anyone published all go on
        /// to upload their own representation of the hash. That is precisely the precondition
        /// for tearing: once more than one writer stores bytes, which representation wins S3 is
        /// decided independently of which one wins the metadata table, so the two can disagree.
        /// Tearing itself is a race and does not surface on every run, so the assertion here is
        /// on the collision; [`legacy_write_order_tears_blob_and_metadata`] pins down the torn
        /// outcome deterministically.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn without_metadata_conditions_writers_upload_over_each_other() {
            let mut collisions = 0;
            let mut tears = 0;

            for _ in 0..64 {
                let state = Arc::new(Mutex::new(FakeState {
                    unprotected: true,
                    ..Default::default()
                }));
                let store = fake_store(state.clone()).await;
                let (_, shared, _) = fragment::generate_random();

                let mut writers = JoinSet::new();
                for index in 0..WRITERS {
                    let store = store.clone();
                    let partition = random::<Partition>();
                    let address = address_with_random_context(shared);
                    let (fragment, payload) = representation(index);

                    lore_base::lore_spawn!(writers, async move {
                        store
                            .put(partition, address, fragment, Some(payload), false)
                            .await
                    });
                }

                while let Some(writer) = writers.join_next().await {
                    let _ = writer.expect("writer panicked");
                }

                let state = state.lock().unwrap();
                if state.uploads > 1 {
                    collisions += 1;
                }
                if cohesion_violation(&state).is_some() {
                    tears += 1;
                }
            }

            assert!(
                collisions > 0,
                "without the metadata conditions nothing serialises the writers, so competing \
                 uploads for a single hash were expected (collisions: {collisions}, torn: {tears})"
            );
        }

        /// One partition stores the content; another later stores the *same* content in a
        /// different representation. The second put must leave the stored blob and its metadata
        /// completely alone, because overwriting the blob would strand the first partition's
        /// content behind metadata that no longer describes it.
        ///
        /// This needs no concurrency at all — it is two sequential puts from different
        /// partitions.
        #[tokio::test]
        async fn cross_partition_put_of_another_representation_leaves_the_blob_alone() {
            let state = Arc::new(Mutex::new(FakeState::default()));
            let store = fake_store(state.clone()).await;
            let (_, shared, _) = fragment::generate_random();

            // The first partition stores the content as one representation.
            let (first, first_payload) = representation(0);
            store
                .clone()
                .put(
                    random::<Partition>(),
                    address_with_random_context(shared),
                    first,
                    Some(first_payload.clone()),
                    false,
                )
                .await
                .expect("the first partition should store the content");

            let (blob, metadata) = {
                let state = state.lock().unwrap();
                (state.object.clone(), state.metadata.clone())
            };
            assert_eq!(
                blob.as_deref(),
                Some(first_payload.as_ref()),
                "the first partition's bytes should be the ones stored"
            );

            // A second partition stores the same content compressed differently.
            let (second, second_payload) = representation(1);
            assert_ne!(
                first.size_payload, second.size_payload,
                "the two partitions must hold genuinely different representations"
            );

            store
                .put(
                    random::<Partition>(),
                    address_with_random_context(shared),
                    second,
                    Some(second_payload),
                    false,
                )
                .await
                .expect("the second partition should deduplicate against the stored content");

            let state = state.lock().unwrap();
            assert_eq!(
                state.object, blob,
                "the stored blob must be untouched by the second partition"
            );
            assert_eq!(
                state.metadata, metadata,
                "the published metadata must be untouched by the second partition"
            );
            assert_blob_and_metadata_agree(&state, "after a cross-partition put");
            assert_eq!(
                state.uploads, 1,
                "the second partition must not upload its own representation over the first's"
            );
        }

        /// The sequence that corrupts the store without this change, end to end.
        ///
        /// One partition stores content and reads it back. A second partition stores the same
        /// content compressed differently, with the next metadata write armed to fail — the
        /// throttling that previously left the first partition reading a blob its own metadata
        /// no longer described.
        ///
        /// Nothing fails now, because nothing is written: the second partition records a
        /// reference to content that is already stored, so there is no upload to replace the
        /// blob and no metadata write to lose.
        #[tokio::test]
        async fn a_failed_cross_partition_write_cannot_corrupt_the_first_partition() {
            let state = Arc::new(Mutex::new(FakeState::default()));
            let store = fake_store(state.clone()).await;
            let (_, shared, _) = fragment::generate_random();

            let first_partition = random::<Partition>();
            let first_address = address_with_random_context(shared);
            let (first_fragment, first_payload) = representation(0);

            store
                .clone()
                .put(
                    first_partition,
                    first_address,
                    first_fragment,
                    Some(first_payload.clone()),
                    false,
                )
                .await
                .expect("the first partition should store the content");

            store
                .clone()
                .get(first_partition, first_address, StoreMatch::MatchFull)
                .await
                .expect("the first partition should be able to read what it stored");

            // Arm the write that used to be lost, then have another partition store the same
            // content in its own representation.
            state.lock().unwrap().fail_next_metadata_write = true;

            let (second_fragment, second_payload) = representation(1);
            store
                .clone()
                .put(
                    random::<Partition>(),
                    address_with_random_context(shared),
                    second_fragment,
                    Some(second_payload),
                    false,
                )
                .await
                .expect("the second partition should deduplicate against the stored content");

            {
                let state = state.lock().unwrap();
                assert!(
                    state.fail_next_metadata_write,
                    "the armed metadata write was consumed, so the second partition published \
                     metadata it had no business publishing"
                );
                assert_eq!(
                    state.uploads, 1,
                    "the second partition uploaded content that was already stored"
                );
            }

            store
                .get(first_partition, first_address, StoreMatch::MatchFull)
                .await
                .expect("the first partition must still be able to read what it stored");
        }

        /// Negative control for the test above, modelling the write order this store used before
        /// deduplication: a put with no exact match uploaded unconditionally, then published its
        /// own metadata. A second partition storing the same content in a different
        /// representation therefore replaced the blob while the first partition's metadata still
        /// described it.
        ///
        /// The pair is inconsistent for the whole gap between those two writes — every read of
        /// the hash fails, including reads from the partition that did not write — and stays that
        /// way permanently if the writer dies before publishing.
        #[tokio::test]
        async fn legacy_cross_partition_overwrite_tears_blob_and_metadata() {
            let state = Arc::new(Mutex::new(FakeState::default()));
            let (first, first_payload) = representation(0);
            let (second, second_payload) = representation(1);
            let (_, shared, _) = fragment::generate_random();

            // The first partition's consistent pair.
            {
                let mut state = state.lock().unwrap();
                state.object = Some(first_payload.to_vec());
                state.metadata = Some(
                    serde_dynamo::to_item(
                        FragmentMetadataEntry::new(shared.hash).with_fragment(first),
                    )
                    .unwrap(),
                );
            }
            assert!(
                cohesion_violation(&state.lock().unwrap()).is_none(),
                "the first partition should start out consistent"
            );

            // The second partition uploads its representation over the top, as the old
            // unconditional put did.
            state.lock().unwrap().object = Some(second_payload.to_vec());

            let torn = cohesion_violation(&state.lock().unwrap());
            assert!(
                torn.is_some(),
                "overwriting the blob should have left it disagreeing with the published metadata"
            );

            // Publishing the second representation restores consistency — so the damage lasts
            // only as long as the gap, unless the writer never gets here.
            state.lock().unwrap().metadata = Some(
                serde_dynamo::to_item(
                    FragmentMetadataEntry::new(shared.hash).with_fragment(second),
                )
                .unwrap(),
            );
            assert!(
                cohesion_violation(&state.lock().unwrap()).is_none(),
                "publishing the second representation should have restored the pair"
            );
        }

        /// Negative control for [`assert_blob_and_metadata_agree`].
        ///
        /// Replays the write order this store used before this change — upload, then publish
        /// metadata unconditionally — with the interleaving that made it unsafe. Both
        /// writers upload and both publish, and because S3 and the metadata table are updated
        /// independently, the writer that wins one is not the writer that wins the other. The
        /// result is a hash whose published metadata describes bytes that are no longer there,
        /// which fails every subsequent read.
        ///
        /// Without this, a cohesion assertion that never fires would look like a passing test.
        #[tokio::test]
        async fn legacy_write_order_tears_blob_and_metadata() {
            let state = Arc::new(Mutex::new(FakeState::default()));
            let (_, shared, _) = fragment::generate_random();

            let (fragment_a, payload_a) = representation(0);
            let (fragment_b, payload_b) = representation(1);
            assert_ne!(
                fragment_a.size_payload, fragment_b.size_payload,
                "the two writers must hold genuinely different representations"
            );

            {
                let mut state = state.lock().unwrap();

                // Both uploads land, B's last...
                state.object = Some(payload_a.to_vec());
                state.object = Some(payload_b.to_vec());

                // ...but the unconditional metadata writes land in the opposite order, so the
                // published fragment is A's while the stored bytes are B's.
                for fragment in [fragment_b, fragment_a] {
                    state.metadata = Some(
                        serde_dynamo::to_item(
                            FragmentMetadataEntry::new(shared.hash).with_fragment(fragment),
                        )
                        .unwrap(),
                    );
                }
            }

            let state = state.lock().unwrap();
            let entry: FragmentMetadataEntry =
                serde_dynamo::from_item(state.metadata.clone().unwrap()).unwrap();
            let published = entry
                .fragment
                .expect("the legacy order publishes a row describing a fragment");
            let object = state.object.as_ref().expect("a blob was uploaded");

            // This is the exact condition `assert_blob_and_metadata_agree` checks first, so the
            // cohesion assertion used by the tests above does detect this state.
            assert_ne!(
                published.size_payload as usize,
                object.len(),
                "the legacy write order should leave the published size describing another \
                 writer's blob"
            );
        }

        /// Once the content is durable, writers holding a *different* representation of it must
        /// deduplicate against what is stored rather than republishing their own description of
        /// it — otherwise the metadata would stop matching the blob.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn deduplicating_writers_adopt_the_stored_representation() {
            let state = Arc::new(Mutex::new(FakeState::default()));
            let store = fake_store(state.clone()).await;
            let (_, shared, _) = fragment::generate_random();

            // One writer stores the content.
            let (fragment, payload) = representation(0);
            store
                .clone()
                .put(
                    random::<Partition>(),
                    address_with_random_context(shared),
                    fragment,
                    Some(payload),
                    false,
                )
                .await
                .expect("initial write should succeed");

            let published = state.lock().unwrap().metadata.clone();

            // Now everyone else stores the same content in their own representation.
            let mut writers = JoinSet::new();
            for index in 1..WRITERS {
                let store = store.clone();
                let partition = random::<Partition>();
                let address = address_with_random_context(shared);
                let (fragment, payload) = representation(index);

                lore_base::lore_spawn!(writers, async move {
                    store
                        .put(partition, address, fragment, Some(payload), false)
                        .await
                });
            }

            while let Some(writer) = writers.join_next().await {
                writer
                    .expect("writer panicked")
                    .expect("deduplicating against durable content should succeed");
            }

            let state = state.lock().unwrap();
            assert_eq!(
                state.uploads, 1,
                "deduplicating writers must not upload their own representation"
            );
            assert_eq!(
                state.metadata, published,
                "deduplicating writers must not republish the payload metadata"
            );
            assert_eq!(
                state.associations.len(),
                usize::from(WRITERS),
                "every writer should have recorded its own association"
            );
            assert_blob_and_metadata_agree(&state, "after deduplication");
        }
    }
}
