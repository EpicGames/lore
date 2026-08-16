// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! DynamoDB implementation of the object-store fragment catalog.

use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::Select;
use lore_base::error::AddressNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_revision::lore_warn;
use lore_revision::util::task_queue::METRICS_TASK_QUEUE_LABEL;
use lore_revision::util::task_queue::TaskQueue;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreQueryResult;
use opentelemetry::KeyValue;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::Instrument;
use tracing::warn;

use super::fragment_catalog::BeginObliteration;
use super::fragment_catalog::FragmentCatalog;
use super::fragment_catalog::ObliterationLease;
use super::fragment_catalog::ReleaseAssociation;
use super::immutable_store::DynamoDbImmutableStoreSettings;
use crate::aws_error::AwsError;
use crate::dynamodb::ConditionParts;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoDbPutCondition;
use crate::dynamodb::DynamoDbQuery;
use crate::dynamodb::error::SdkError as DynamoDbSdkError;

const FRAGMENTS_PARTITION_KEY: &str = "hash";
const FRAGMENTS_SORT_KEY: &str = "repository_context";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AssociationEntry {
    pub(crate) hash: Hash,
    #[serde(with = "serde_bytes")]
    pub(crate) repository_context: [u8; size_of::<Context>() * 2],
}

impl Debug for AssociationEntry {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssociationEntry")
            .field("hash", &self.hash)
            .field("repository_context", &hex::encode(self.repository_context))
            .finish()
    }
}

impl From<&AssociationEntry> for Address {
    fn from(entry: &AssociationEntry) -> Self {
        Self {
            hash: entry.hash,
            context: Context::from(&entry.repository_context[size_of::<Context>()..]),
        }
    }
}

impl AssociationEntry {
    pub(crate) fn new(repository: Context, address: Address) -> Self {
        let mut repository_context = [0u8; size_of::<Context>() * 2];
        repository_context[..size_of::<Context>()].copy_from_slice(repository.as_ref());
        repository_context[size_of::<Context>()..].copy_from_slice(address.context.as_ref());
        Self {
            hash: address.hash,
            repository_context,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct MetadataEntry {
    pub(crate) hash: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    pub(crate) fragment: Option<Fragment>,
}

impl MetadataEntry {
    pub(crate) fn new(hash: Hash) -> Self {
        Self {
            hash,
            fragment: None,
        }
    }

    pub(crate) fn with_fragment(mut self, fragment: Fragment) -> Self {
        self.fragment = Some(fragment);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AssociationQuery {
    Repository(Hash, Context),
    Hash(Hash),
    HashCount(Hash),
}

impl DynamoDbQuery for AssociationQuery {
    fn key_condition_expression(&self) -> &str {
        match self {
            Self::Repository(_, _) => "#pk = :hash and begins_with(#sk, :repository)",
            Self::Hash(_) | Self::HashCount(_) => "#pk = :hash",
        }
    }

    fn expression_attribute_names(&self) -> HashMap<String, String> {
        match self {
            Self::Repository(_, _) => HashMap::from([
                ("#pk".to_string(), FRAGMENTS_PARTITION_KEY.to_string()),
                ("#sk".to_string(), FRAGMENTS_SORT_KEY.to_string()),
            ]),
            Self::Hash(_) | Self::HashCount(_) => {
                HashMap::from([("#pk".to_string(), FRAGMENTS_PARTITION_KEY.to_string())])
            }
        }
    }

    fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
        match self {
            Self::Repository(hash, repository) => HashMap::from([
                (
                    ":hash".to_string(),
                    AttributeValue::B(Blob::new(hash.as_ref())),
                ),
                (
                    ":repository".to_string(),
                    AttributeValue::B(Blob::new(repository.as_ref())),
                ),
            ]),
            Self::Hash(hash) | Self::HashCount(hash) => HashMap::from([(
                ":hash".to_string(),
                AttributeValue::B(Blob::new(hash.as_ref())),
            )]),
        }
    }

    fn limit(&self) -> Option<i32> {
        match self {
            Self::Repository(_, _) | Self::Hash(_) => Some(1),
            Self::HashCount(_) => None,
        }
    }

    fn select(&self) -> Option<Select> {
        match self {
            Self::Repository(_, _) | Self::Hash(_) => None,
            Self::HashCount(_) => Some(Select::Count),
        }
    }

    fn consistent_read(&self) -> bool {
        matches!(self, Self::HashCount(_))
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct MetadataCondition(pub(crate) Fragment);

impl DynamoDbPutCondition for MetadataCondition {
    fn into_parts(self) -> ConditionParts {
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

type BatchTaskResult = Result<(usize, StoreMatch), (usize, StoreError)>;

/// DynamoDB metadata and association catalog used by the legacy AWS composition.
pub struct DynamoDbFragmentCatalog {
    dynamodb: DynamoDb,
    task_queue: TaskQueue<BatchTaskResult>,
    fragments_table_name: Arc<str>,
    metadata_table_name: Arc<str>,
}

impl DynamoDbFragmentCatalog {
    /// Create a catalog over existing DynamoDB tables.
    pub fn new(
        dynamodb: DynamoDb,
        settings: &DynamoDbImmutableStoreSettings,
        batch_submission_limit: usize,
    ) -> Self {
        Self {
            dynamodb,
            task_queue: TaskQueue::new(
                u32::MAX,
                Semaphore::MAX_PERMITS,
                batch_submission_limit,
                vec![KeyValue::new(
                    METRICS_TASK_QUEUE_LABEL,
                    "store.immutable.aws.catalog.dynamodb",
                )],
            ),
            fragments_table_name: Arc::from(settings.fragments_table_name.clone()),
            metadata_table_name: Arc::from(settings.metadata_table_name.clone()),
        }
    }

    async fn exists_exact(&self, entry: &AssociationEntry) -> Result<bool, StoreError> {
        let item = serde_dynamo::to_item(entry).map_err(|error| {
            StoreError::internal_with_context(
                error,
                "Failed to serialize fragment association for DynamoDB lookup",
            )
        })?;
        self.dynamodb
            .get_item(&self.fragments_table_name, item, true)
            .await
            .map(|output| output.item.is_some())
            .map_err(dynamodb_operation_error)
    }

    async fn exists_repository(&self, entry: &AssociationEntry) -> Result<bool, StoreError> {
        let repository = Context::from(&entry.repository_context[..size_of::<Context>()]);
        self.dynamodb
            .query_single(
                &self.fragments_table_name,
                AssociationQuery::Repository(entry.hash, repository),
            )
            .await
            .map(|output| output.count > 0)
            .map_err(dynamodb_operation_error)
    }

    async fn exists_hash(&self, entry: &AssociationEntry) -> Result<bool, StoreError> {
        self.dynamodb
            .query_single(
                &self.fragments_table_name,
                AssociationQuery::Hash(entry.hash),
            )
            .await
            .map(|output| output.count > 0)
            .map_err(dynamodb_operation_error)
    }

    async fn exists(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<bool, StoreError> {
        let entry = AssociationEntry::new(repository, address);
        match match_requested {
            StoreMatch::MatchFull => self.exists_exact(&entry).await,
            StoreMatch::MatchPartition => self.exists_repository(&entry).await,
            StoreMatch::MatchHash => self.exists_hash(&entry).await,
            StoreMatch::MatchNone => Ok(false),
        }
    }

    async fn lookup(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        let mut current = match_requested;
        let mut exists = self.exists(repository, address, current).await?;
        if !exists && current == StoreMatch::MatchFull {
            return Ok(StoreMatch::MatchNone);
        }
        while !exists {
            let Some(previous) = current.prev() else {
                break;
            };
            current = previous;
            exists = self.exists(repository, address, current).await?;
        }
        Ok(if exists {
            current
        } else {
            StoreMatch::MatchNone
        })
    }

    async fn query_batch_exact(
        &self,
        repository: Context,
        addresses: &[Address],
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let mut index = HashMap::with_capacity(addresses.len());
        let mut items = Vec::with_capacity(addresses.len());
        for (position, address) in addresses.iter().copied().enumerate() {
            index.insert(address, position);
            items.push(
                serde_dynamo::to_item(AssociationEntry::new(repository, address)).map_err(
                    |error| {
                        StoreError::internal_with_context(
                            error,
                            "Failed to serialize DynamoDB batch association lookup",
                        )
                    },
                )?,
            );
        }
        let rows = self
            .dynamodb
            .batch_get_item(&self.fragments_table_name, items, true)
            .await
            .map_err(dynamodb_operation_error)?;
        let mut output = vec![StoreMatch::MatchNone; addresses.len()];
        for row in rows {
            let entry: AssociationEntry = serde_dynamo::from_item(row).map_err(|error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to deserialize DynamoDB batch association",
                )
            })?;
            let address = Address {
                hash: entry.hash,
                context: Context::from(&entry.repository_context[size_of::<Context>()..]),
            };
            if let Some(position) = index.get(&address) {
                output[*position] = StoreMatch::MatchFull;
            }
        }
        Ok(output)
    }

    async fn query_batch_inexact(
        &self,
        repository: Context,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let mut joins = JoinSet::new();
        for (position, address) in addresses.iter().copied().enumerate() {
            let dynamodb = self.dynamodb.clone();
            let table = self.fragments_table_name.clone();
            let task = async move {
                let query = match match_requested {
                    StoreMatch::MatchPartition => {
                        AssociationQuery::Repository(address.hash, repository)
                    }
                    StoreMatch::MatchHash => AssociationQuery::Hash(address.hash),
                    _ => unreachable!("inexact lookup requires partition or hash match"),
                };
                dynamodb
                    .query_single(&table, query)
                    .await
                    .map(|result| {
                        (
                            position,
                            if result.count > 0 {
                                match_requested
                            } else {
                                StoreMatch::MatchNone
                            },
                        )
                    })
                    .map_err(|error| (position, dynamodb_operation_error(error)))
            }
            .in_current_span();
            lore_base::lore_spawn!(
                joins,
                self.task_queue
                    .submit(Box::pin(task))
                    .await
                    .map_err(|error| {
                        lore_warn!("Task queue error: {error}");
                        StoreError::internal_with_context(
                            error,
                            "Failed to submit DynamoDB batch lookup",
                        )
                    })?
                    .in_current_span()
            );
        }

        let mut output = vec![StoreMatch::MatchNone; addresses.len()];
        while let Some(joined) = joins.join_next().await {
            let task = joined.map_err(|error| {
                StoreError::internal_with_context(error, "DynamoDB batch lookup task failed")
            })?;
            match task.map_err(|error| {
                StoreError::internal_with_context(error, "DynamoDB task queue failed")
            })? {
                Ok((position, matched)) => output[position] = matched,
                Err((position, error)) => {
                    warn!(
                        ?error,
                        address = %addresses[position],
                        "DynamoDB batch member lookup failed; returning no match"
                    );
                }
            }
        }
        Ok(output)
    }

    async fn write_metadata(&self, hash: Hash, fragment: Fragment) -> Result<(), StoreError> {
        let item = serde_dynamo::to_item(MetadataEntry::new(hash).with_fragment(fragment))
            .map_err(|error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to serialize DynamoDB fragment metadata",
                )
            })?;
        self.dynamodb
            .put_item(&self.metadata_table_name, item)
            .await
            .map(|_| ())
            .map_err(dynamodb_operation_error)
    }

    async fn update_metadata(
        &self,
        hash: Hash,
        updated: Fragment,
        expected: Fragment,
    ) -> Result<(), StoreError> {
        let item = serde_dynamo::to_item(MetadataEntry::new(hash).with_fragment(updated)).map_err(
            |error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to serialize conditional DynamoDB metadata update",
                )
            },
        )?;
        match self
            .dynamodb
            .put_item_conditional(&self.metadata_table_name, item, MetadataCondition(expected))
            .await
        {
            Ok(_) => Ok(()),
            Err(AwsError::AwsSdkError(DynamoDbSdkError::ServiceError(error)))
                if error.err().is_conditional_check_failed_exception() =>
            {
                if let PutItemError::ConditionalCheckFailedException(exception) = error.err() {
                    warn!(actual = ?exception.item(), ?expected, "DynamoDB metadata conflict");
                }
                Err(StoreError::internal(
                    "Failed to update fragment metadata due to conflict",
                ))
            }
            Err(error) => Err(dynamodb_operation_error(error)),
        }
    }

    async fn put_association(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError> {
        let item =
            serde_dynamo::to_item(AssociationEntry::new(repository, address)).map_err(|error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to serialize DynamoDB fragment association",
                )
            })?;
        self.dynamodb
            .put_item(&self.fragments_table_name, item)
            .await
            .map(|_| ())
            .map_err(dynamodb_operation_error)
    }

    async fn delete_association(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError> {
        let key =
            serde_dynamo::to_item(AssociationEntry::new(repository, address)).map_err(|error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to serialize DynamoDB fragment association delete",
                )
            })?;
        self.dynamodb
            .delete_item(&self.fragments_table_name, key)
            .await
            .map(|_| ())
            .map_err(dynamodb_operation_error)
    }

    async fn has_associations(&self, hash: Hash) -> Result<bool, StoreError> {
        self.dynamodb
            .query_single(
                &self.fragments_table_name,
                AssociationQuery::HashCount(hash),
            )
            .await
            .map(|output| output.count > 0)
            .map_err(dynamodb_operation_error)
    }
}

#[async_trait]
impl FragmentCatalog for DynamoDbFragmentCatalog {
    async fn exist(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        self.lookup(repository, address, match_requested).await
    }

    async fn query(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        let match_made = self.lookup(repository, address, match_requested).await?;
        if match_made == StoreMatch::MatchNone {
            return Ok(StoreQueryResult::default());
        }
        Ok(StoreQueryResult {
            fragment: self.load_metadata(address.hash).await?,
            match_made,
        })
    }

    async fn query_batch(
        &self,
        repository: Context,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        match match_requested {
            StoreMatch::MatchNone => Ok(vec![StoreMatch::MatchNone; addresses.len()]),
            StoreMatch::MatchFull => self.query_batch_exact(repository, addresses).await,
            StoreMatch::MatchPartition | StoreMatch::MatchHash => {
                self.query_batch_inexact(repository, addresses, match_requested)
                    .await
            }
        }
    }

    async fn load_metadata(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let key = serde_dynamo::to_item(MetadataEntry::new(hash)).map_err(|error| {
            StoreError::internal_with_context(
                error,
                "Failed to serialize DynamoDB fragment metadata key",
            )
        })?;
        let item = self
            .dynamodb
            .get_item(&self.metadata_table_name, key, true)
            .await
            .map_err(|error| {
                if let AwsError::AwsSdkError(SdkError::TimeoutError(_)) = error {
                    StoreError::from(SlowDown)
                } else {
                    StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
                }
            })?
            .item
            .ok_or_else(|| {
                StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
            })?;
        serde_dynamo::from_item::<_, MetadataEntry>(item)
            .map_err(|error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to deserialize DynamoDB fragment metadata",
                )
            })?
            .fragment
            .ok_or_else(|| StoreError::internal("Fragment metadata entry is incomplete"))
    }

    async fn register_fragment(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
    ) -> Result<(), StoreError> {
        // Preserve the legacy DynamoDB ordering. The object-store state machine performs the
        // collision/marker lookup before registration; PostgreSQL strengthens this to one catalog
        // transaction in its implementation.
        self.write_metadata(address.hash, fragment).await?;
        self.put_association(repository, address).await
    }

    async fn associate_fragment(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError> {
        self.put_association(repository, address).await
    }

    async fn begin_obliteration(&self, hash: Hash) -> Result<BeginObliteration, StoreError> {
        let stored = self.load_metadata(hash).await?;
        lore_storage::validate_fragment_size(&stored)?;
        if stored.flags & FragmentFlags::PayloadObliterated.bits() != 0 {
            return Ok(BeginObliteration::AlreadyObliterated);
        }
        let mut original = stored;
        original.flags &= !OBLITERATION_MASK;
        let marker = if stored.flags & FragmentFlags::PayloadObliterating.bits() != 0 {
            stored
        } else {
            let mut marker = original;
            marker.flags |= FragmentFlags::PayloadObliterating.bits();
            self.update_metadata(hash, marker, original).await?;
            marker
        };
        Ok(BeginObliteration::Acquired(ObliterationLease::new(
            original, marker,
        )))
    }

    async fn release_association(
        &self,
        repository: Context,
        address: Address,
        lease: ObliterationLease,
    ) -> Result<ReleaseAssociation, StoreError> {
        self.delete_association(repository, address).await?;
        if self.has_associations(address.hash).await? {
            self.update_metadata(address.hash, lease.original(), lease.marker())
                .await?;
            Ok(ReleaseAssociation::ReferencesRemain)
        } else {
            Ok(ReleaseAssociation::PayloadUnreferenced)
        }
    }

    async fn finalize_obliteration(
        &self,
        hash: Hash,
        lease: ObliterationLease,
    ) -> Result<(), StoreError> {
        let terminal = Fragment {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 0,
            size_content: 0,
        };
        self.update_metadata(hash, terminal, lease.marker()).await
    }

    fn max_query_batch(&self) -> Option<usize> {
        Some(crate::dynamodb::BATCH_GET_ITEM_MAX_COUNT)
    }
}

const OBLITERATION_MASK: u32 = FragmentFlags::PayloadObliteration.bits();

fn dynamodb_operation_error<E>(error: AwsError<E>) -> StoreError
where
    E: std::error::Error + Send + Sync + 'static,
{
    warn!(?error, "DynamoDB fragment catalog operation failed");
    if matches!(error, AwsError::AwsSdkError(_)) {
        StoreError::from(SlowDown)
    } else {
        StoreError::internal_with_context(error, "DynamoDB fragment catalog operation failed")
    }
}
