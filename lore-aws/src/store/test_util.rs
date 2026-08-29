// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemOutput;
use aws_sdk_dynamodb::operation::get_item::GetItemError;
use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
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
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use tokio::sync::oneshot;

use crate::aws_error::AwsError;
use crate::dynamodb::MockDynamoDb;
use crate::s3::MockS3Impl;
use crate::store::immutable_store::AwsImmutableStore;
use crate::store::immutable_store::AwsImmutableStoreSettings;
use crate::store::immutable_store::DynamoDbImmutableStoreSettings;
use crate::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
use crate::store::immutable_store::FragmentState;
use crate::store::immutable_store::FragmentStateEntry;
use crate::store::immutable_store::FragmentsEntry;
use crate::store::immutable_store::FragmentsQuery;
use crate::store::immutable_store::PartitionAssociationQuery;
use crate::store::immutable_store::RowAbsent;
use crate::store::immutable_store::S3StoreSettings;
use crate::store::immutable_store::StateUnchanged;
use crate::store::object_metadata::from_object_metadata;

pub(crate) const BUCKET: &str = "test-bucket";
pub(crate) const FRAGMENTS_TABLE_NAME: &str = "fragments";
pub(crate) const FRAGMENT_STATE_TABLE_NAME: &str = "fragment-state";
/// A separate table name for legacy fragment metadata, distinct from the state table. Used
/// to test the `do_query` path that falls back to the metadata table when no state row exists.
pub(crate) const FRAGMENT_METADATA_TABLE_NAME: &str = "fragment-metadata";

/// A stored object: its body, and the object metadata written with it.
pub(crate) type StoredObject = (Vec<u8>, HashMap<String, String>);

/// An operation the fake can be told to fail, so error paths are reachable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Fault {
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

/// An in-memory stand-in for the bucket and the two tables.
///
/// The tests are written against behaviour rather than a call sequence: they put and get
/// through the real store and assert on what ends up stored. That is what lets the concurrency
/// test exist at all — a mock programmed with an expected order of calls cannot express
/// "any interleaving, and the result must still be coherent".
#[derive(Default)]
pub(crate) struct Storage {
    pub(crate) faults: HashSet<Fault>,
    pub(crate) race_state: Option<(Hash, FragmentState)>,
    /// Fired when an obliteration deletes its association, so a task can land one of its own
    /// in the window that follows.
    pub(crate) association_deleted: Option<oneshot::Sender<()>>,
    pub(crate) object_reads: usize,
    pub(crate) objects: HashMap<Vec<u8>, StoredObject>,
    /// Objects returned by `get_object` exactly once before being removed. Not visible to
    /// `head_object`, so a test can make `head_fragment` fail while `load` still succeeds.
    pub(crate) objects_once: HashMap<Vec<u8>, StoredObject>,
    pub(crate) associations: HashMap<(Vec<u8>, Vec<u8>), HashMap<String, AttributeValue>>,
    pub(crate) state: HashMap<Vec<u8>, HashMap<String, AttributeValue>>,
    /// Rows in the legacy fragment metadata table (separate from the state table). Only
    /// populated by `set_legacy_metadata_row`, which is used by tests that need to exercise
    /// the `do_query` branch that falls back to the metadata table when there is no
    /// state row.
    pub(crate) legacy_metadata: HashMap<Vec<u8>, HashMap<String, AttributeValue>>,
}

#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<Storage>>);

impl Fake {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Storage> {
        self.0.lock().unwrap()
    }

    /// Make `fault` fail from now on. Latched rather than one-shot, so a retrying caller sees a
    /// persistent failure rather than one that heals underneath it.
    pub(crate) fn fail(&self, fault: Fault) {
        self.lock().faults.insert(fault);
    }

    /// Move `hash` into `state` at the moment its payload is uploaded, so a put reaches its
    /// publish step having probed before an obliteration and uploaded after it. That window
    /// cannot be hit by ordering calls from the outside.
    pub(crate) fn obliterate_during_upload(&self, hash: Hash, state: FragmentState) {
        self.lock().race_state = Some((hash, state));
    }

    pub(crate) fn failing(&self, fault: Fault) -> bool {
        self.lock().faults.contains(&fault)
    }

    pub(crate) fn object_reads(&self) -> usize {
        self.lock().object_reads
    }

    pub(crate) fn object(&self, hash: Hash) -> Option<StoredObject> {
        self.lock()
            .objects
            .get(&hash.to_string().into_bytes())
            .cloned()
    }

    pub(crate) fn stored_fragment(&self, hash: Hash) -> Option<Fragment> {
        self.object(hash)
            .map(|(_, metadata)| from_object_metadata(Some(&metadata)).unwrap())
    }

    pub(crate) fn state_of(&self, hash: Hash) -> Option<FragmentState> {
        self.lock()
            .state
            .get(hash.data().as_slice())
            .map(|item| serde_dynamo::from_item::<_, FragmentStateEntry>(item.clone()).unwrap())
            .map(|entry| entry.state())
    }

    pub(crate) fn association_count(&self, hash: Hash) -> usize {
        self.lock()
            .associations
            .keys()
            .filter(|(stored, _)| stored == hash.data())
            .count()
    }

    pub(crate) fn set_state(&self, hash: Hash, state: FragmentState) {
        let item = serde_dynamo::to_item(FragmentStateEntry::new(hash, state)).unwrap();
        self.lock().state.insert(hash.data().to_vec(), item);
    }

    /// Write a row in the shape used before fragments moved onto the object: no `state`, and a
    /// whole flattened fragment whose `flags` also carry the obliteration bits.
    pub(crate) fn set_fragment_metadata_row(&self, hash: Hash, fragment: Fragment) {
        use aws_sdk_dynamodb::primitives::Blob;
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
    pub(crate) fn set_legacy_metadata_row(&self, hash: Hash, fragment: Fragment) {
        use aws_sdk_dynamodb::primitives::Blob;
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
    pub(crate) fn lose_object(&self, hash: Hash) {
        self.lock().objects.remove(&hash.to_string().into_bytes());
    }

    /// Store an object the way one was stored before the fragment moved onto it: bare bytes,
    /// no fragment metadata.
    pub(crate) fn put_object_without_metadata(&self, hash: Hash, body: &[u8]) {
        self.lock().objects.insert(
            hash.to_string().into_bytes(),
            (body.to_vec(), HashMap::new()),
        );
    }

    /// Store an object that `get_object` will return exactly once before removing it.
    /// Not visible to `head_object`, so `head_fragment` returns 404 while `load` still succeeds.
    pub(crate) fn put_object_once(&self, hash: Hash, body: &[u8]) {
        self.lock().objects_once.insert(
            hash.to_string().into_bytes(),
            (body.to_vec(), HashMap::new()),
        );
    }

    /// Store an object with the `lore-fragment` metadata header encoded from `fragment`.
    pub(crate) fn put_object_with_fragment_metadata(
        &self,
        hash: Hash,
        body: &[u8],
        fragment: Fragment,
    ) {
        self.lock().objects.insert(
            hash.to_string().into_bytes(),
            (
                body.to_vec(),
                crate::store::object_metadata::to_object_metadata(&fragment),
            ),
        );
    }

    /// Store an object whose metadata is present but unreadable.
    pub(crate) fn put_object_with_damaged_metadata(&self, hash: Hash, body: &[u8]) {
        let mut metadata = HashMap::new();
        metadata.insert("lore-fragment".to_owned(), "not:a:fragment".to_owned());

        self.lock()
            .objects
            .insert(hash.to_string().into_bytes(), (body.to_vec(), metadata));
    }

    /// Signals when an obliteration deletes its association, so a caller can land its own
    /// between that delete and the re-count — the window the drain exists to cover. Ordering
    /// calls from the outside cannot hit it.
    pub(crate) fn association_deleted(&self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.lock().association_deleted = Some(sender);
        receiver
    }

    pub(crate) fn has_association(&self, partition: Partition, address: Address) -> bool {
        let entry = FragmentsEntry::new(partition, address);
        self.lock()
            .associations
            .contains_key(&(entry.hash.data().to_vec(), entry.partition_context.to_vec()))
    }

    pub(crate) fn add_association(&self, partition: Partition, address: Address) {
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
pub(crate) fn wire(fake: &Fake) -> (MockS3Impl, MockDynamoDb) {
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::operation::head_object::HeadObjectError;

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
        if let Some((body, metadata)) = storage.objects_once.remove(key.as_bytes()) {
            let len = body.len() as i64;
            return Ok(GetObjectOutput::builder()
                .set_body(Some(body.into()))
                .set_metadata(Some(metadata))
                .content_length(len)
                .build());
        }
        match storage.objects.get(key.as_bytes()) {
            Some((body, metadata)) => Ok(GetObjectOutput::builder()
                .set_body(Some(body.clone().into()))
                .set_metadata(Some(metadata.clone()))
                .content_length(body.len() as i64)
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
    dynamodb.expect_get_item().returning(move |table, item, _| {
        if &**table == FRAGMENT_STATE_TABLE_NAME {
            if f.failing(Fault::StateReadTimeout) {
                return Err(AwsError::sdk_error(SdkError::timeout_error(Box::new(
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
                    GetItemError::ProvisionedThroughputExceededException(throttling_exception()),
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
                .get(&(
                    blob(&item, "hash"),
                    blob(&item, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
                ))
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
                    PutItemError::ProvisionedThroughputExceededException(throttling_exception()),
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
        let FragmentsQuery(hash) = query;
        let matched = storage
            .associations
            .keys()
            .any(|(stored, _)| stored == hash.data());

        // The query limits itself to one row, so the service could never report more than one no
        // matter how many partitions hold the hash. Reporting the true total here would let a
        // caller that needs an actual count pass against this fake and under-report against S3.
        Ok(QueryOutput::builder().count(i32::from(matched)).build())
    });

    let f = fake.clone();
    dynamodb.expect_query_single().returning(move |_, query| {
        if f.failing(Fault::AssociationCount) {
            return Err(throughput_exceeded(
                QueryError::ProvisionedThroughputExceededException(throttling_exception()),
            ));
        }

        let storage = f.lock();
        let PartitionAssociationQuery(hash, partition) = query;
        // The service's `begins_with` on a sort key of partition followed by context.
        let matched = storage.associations.keys().any(|(stored, sort_key)| {
            stored == hash.data() && sort_key.starts_with(partition.data())
        });

        Ok(QueryOutput::builder().count(i32::from(matched)).build())
    });

    (s3, dynamodb)
}

pub(crate) async fn store_with(
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
        dynamodb_settings =
            dynamodb_settings.with_fragment_metadata_table(FRAGMENT_STATE_TABLE_NAME.to_string());
    }

    let settings = AwsImmutableStoreSettings {
        s3: S3StoreSettings::new(BUCKET.to_string()),
        dynamodb: dynamodb_settings,
        force_write,
    };

    let execution = super::setup_execution("test".to_string());
    LORE_CONTEXT
        .scope(execution, async move {
            Arc::new(AwsImmutableStore::new(s3, dynamodb, &settings))
        })
        .await
}

pub(crate) async fn store(fake: &Fake) -> Arc<AwsImmutableStore> {
    store_with(fake, false, false).await
}

/// A store on a deployment that may still hold objects written before fragments moved onto
/// them, and so is configured to read the rows describing those.
pub(crate) async fn migrated_store(fake: &Fake) -> Arc<AwsImmutableStore> {
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
pub(crate) async fn store_with_separate_metadata_table(fake: &Fake) -> Arc<AwsImmutableStore> {
    let (s3, dynamodb) = wire(fake);
    let mut dynamodb_settings = DynamoDbImmutableStoreSettings::new(
        FRAGMENTS_TABLE_NAME.to_string(),
        FRAGMENT_STATE_TABLE_NAME.to_string(),
    );
    dynamodb_settings.timeout_millis = 1;
    dynamodb_settings =
        dynamodb_settings.with_fragment_metadata_table(FRAGMENT_METADATA_TABLE_NAME.to_string());

    let settings = AwsImmutableStoreSettings {
        s3: S3StoreSettings::new(BUCKET.to_string()),
        dynamodb: dynamodb_settings,
        force_write: false,
    };

    let execution = super::setup_execution("test".to_string());
    LORE_CONTEXT
        .scope(execution, async move {
            Arc::new(AwsImmutableStore::new(s3, dynamodb, &settings))
        })
        .await
}

pub(crate) fn aws_error<E>(error: E, status: u16) -> AwsError<SdkError<E, HttpResponse>> {
    AwsError::sdk_error(SdkError::ServiceError(
        ServiceError::builder()
            .source(error)
            .raw(HttpResponse::new(
                status.try_into().unwrap(),
                SdkBody::empty(),
            ))
            .build(),
    ))
}

pub(crate) fn blob(item: &HashMap<String, AttributeValue>, key: &str) -> Vec<u8> {
    item.get(key)
        .and_then(|value| value.as_b().ok())
        .map(|value| value.as_ref().to_vec())
        .unwrap_or_default()
}

/// A throttling exception carrying the error code a real response would, which is what the
/// classifier reads — a builder-constructed exception has no metadata at all.
pub(crate) fn throttling_exception() -> ProvisionedThroughputExceededException {
    ProvisionedThroughputExceededException::builder()
        .meta(
            ErrorMetadata::builder()
                .code("ProvisionedThroughputExceededException")
                .build(),
        )
        .build()
}

pub(crate) fn throughput_exceeded<E>(error: E) -> AwsError<SdkError<E, HttpResponse>> {
    aws_error(error, 400)
}

pub(crate) fn conditional_check_failed(
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

/// A payload and the fragment that correctly describes it.
pub(crate) fn representation(
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
