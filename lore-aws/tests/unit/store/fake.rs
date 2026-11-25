// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use http::StatusCode;
use lore_aws::aws::dynamodb::GetItemOutput;
use lore_aws::aws::dynamodb::QueryOutput;
use lore_aws::aws::dynamodb::WriteOutput;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::error::ErrorPayload;
use lore_aws::aws::error::S3ErrorCode;
use lore_aws::aws::error::ServiceError;
use lore_aws::aws::error::ServiceErrorCode;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::item::from_item;
use lore_aws::aws::item::to_item;
use lore_aws::aws::s3::ByteStream;
use lore_aws::aws::s3::GetObjectOutput;
use lore_aws::aws::s3::HeadObjectOutput;
use lore_aws::aws::s3::ListObjectVersionsOutput;
use lore_aws::aws::s3::ObjectMetadata;
use lore_aws::aws::s3::ObjectVersion;
use lore_aws::aws::s3::WriteOutput as S3WriteOutput;
use lore_aws::aws_error::AwsError;
use lore_aws::dynamodb::DynamoError;
use lore_aws::dynamodb::MockDynamoDb;
use lore_aws::s3::MockS3Impl;
use lore_aws::s3::S3Error;
use lore_aws::store::immutable_store::AwsImmutableStore;
use lore_aws::store::immutable_store::AwsImmutableStoreSettings;
use lore_aws::store::immutable_store::DynamoDbImmutableStoreSettings;
use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
use lore_aws::store::immutable_store::FragmentState;
use lore_aws::store::immutable_store::FragmentStateEntry;
use lore_aws::store::immutable_store::FragmentsEntry;
use lore_aws::store::immutable_store::FragmentsQuery;
use lore_aws::store::immutable_store::PartitionAssociationQuery;
use lore_aws::store::immutable_store::RowAbsent;
use lore_aws::store::immutable_store::S3StoreSettings;
use lore_aws::store::immutable_store::StateUnchanged;
use lore_aws::store::object_metadata::from_object_metadata;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use tokio::sync::oneshot;

pub(crate) const BUCKET: &str = "test-bucket";
pub(crate) const FRAGMENTS_TABLE_NAME: &str = "fragments";
pub(crate) const FRAGMENT_STATE_TABLE_NAME: &str = "fragment-state";
/// A separate table name for legacy fragment metadata, distinct from the state table. Used
/// to test the `do_query` path that falls back to the metadata table when no state row exists.
pub(crate) const FRAGMENT_METADATA_TABLE_NAME: &str = "fragment-metadata";

/// A stored object: its body, and the object metadata written with it.
pub(crate) type StoredObject = (Vec<u8>, ObjectMetadata);

/// The version id S3 gives an object written to a bucket without versioning.
pub(crate) const NULL_VERSION: &str = "null";

/// How much of a body [`Fault::ObjectReadChunked`] hands over per read.
///
/// The largest payload a fragment may hold, which is the largest piece worth handing over at once: a
/// transport read is bounded by the HTTP client's own buffer rather than by anything the response
/// declares, and that buffer is of this order. Anything smaller would have the fake model a gentler
/// transport than the real one.
pub(crate) const CHUNK_SIZE: usize = FRAGMENT_SIZE_THRESHOLD;

/// An operation the fake can be told to fail, so error paths are reachable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Fault {
    StateRead,
    StateReadTimeout,
    StateReadBroken,
    /// Batch reads fail as if their keys were still unprocessed after the resend limit.
    BatchReadUnprocessed,
    /// Every state write conditional on the row's current state, shed.
    StateWrite,
    /// Such a write releasing an obliteration mark back to `Stored`, shed.
    StateRelease,
    /// Such a write finalizing an obliteration mark as `Obliterated`, shed.
    StateFinalize,
    StateDelete,
    /// A keyed read of one association, shed.
    AssociationRead,
    AssociationWrite,
    AssociationDelete,
    AssociationCount,
    ObjectDelete,
    ObjectList,
    /// A version listing that reports it stopped short of the end.
    ObjectListTruncated,
    /// A `PutObject` the service sheds.
    ObjectWriteShed,
    /// A `PutObject` the service refuses, which is a misconfiguration rather than congestion.
    ObjectWriteDenied,
    /// A `HeadObject` the service refuses.
    ObjectHeadDenied,
    /// A `GetObject` whose body stops partway, as a reset connection leaves it.
    ObjectReadTruncated,
    /// A `GetObject` answered as a chunked reply: no declared length, and a body in pieces.
    ObjectReadChunked,
    /// A `GetObject` the service refuses, which is a misconfiguration rather than congestion.
    ObjectReadDenied,
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
    /// Faults that fail the next check of them only.
    pub(crate) faults_once: HashSet<Fault>,
    pub(crate) race_state: Option<(Hash, FragmentState)>,
    /// Fired when an obliteration deletes its association, so a task can land one of its own
    /// in the window that follows.
    pub(crate) association_deleted: Option<oneshot::Sender<()>>,
    pub(crate) object_reads: usize,
    /// Keyed reads of one state row, failed ones included.
    pub(crate) state_reads: usize,
    pub(crate) objects: HashMap<Vec<u8>, StoredObject>,
    /// Objects returned by `get_object` exactly once before being removed. Not visible to
    /// `head_object`, so a test can make `head_fragment` fail while `load` still succeeds.
    pub(crate) objects_once: HashMap<Vec<u8>, StoredObject>,
    /// Every version of every key, in the order written, as `ListObjectVersions` reports them.
    pub(crate) versions: Vec<ObjectVersion>,
    pub(crate) associations: HashMap<(Vec<u8>, Vec<u8>), Item>,
    pub(crate) state: HashMap<Vec<u8>, Item>,
    /// Rows in the legacy fragment metadata table (separate from the state table). Only
    /// populated by `set_legacy_metadata_row`, which is used by tests that need to exercise
    /// the `do_query` branch that falls back to the metadata table when there is no
    /// state row.
    pub(crate) legacy_metadata: HashMap<Vec<u8>, Item>,
}

impl Storage {
    /// Store `object` under `key` as a plain put does on a bucket without versioning: it becomes
    /// the key's `null` version, replacing any earlier one.
    fn put(&mut self, key: &str, object: StoredObject) {
        self.versions
            .retain(|version| !(version.key.as_str() == key && is_null(version)));
        for version in self
            .versions
            .iter_mut()
            .filter(|version| version.key.as_str() == key)
        {
            version.is_latest = false;
        }

        self.versions.push(ObjectVersion {
            key: ByteStr::copy_from_str(key),
            version_id: Some(ByteStr::from_static(NULL_VERSION)),
            is_delete_marker: false,
            is_latest: true,
        });
        self.objects.insert(key.as_bytes().to_vec(), object);
    }
}

fn is_null(version: &ObjectVersion) -> bool {
    version.version_id.as_deref() == Some(NULL_VERSION)
}

#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<Storage>>);

/// The single-key read behind both `get_item` and `item_exists`, routed by table.
///
/// One function so the two cannot drift: in production they issue the same request and differ only in
/// how much of the response is decoded, and the fake should not be the place that stops being true.
fn read_one(fake: &Fake, table: &str, item: &Item) -> Result<Option<Item>, DynamoError> {
    if table == FRAGMENTS_TABLE_NAME && fake.failing(Fault::AssociationRead) {
        return Err(throughput_exceeded());
    }

    if table == FRAGMENT_STATE_TABLE_NAME {
        fake.lock().state_reads += 1;
        if fake.failing(Fault::StateReadTimeout) {
            return Err(AwsError::Aws(AwsCallError::Timeout));
        }
        if fake.failing(Fault::StateReadBroken) {
            return Err(dynamo_error(
                DynamoErrorCode::ResourceNotFoundException,
                StatusCode::BAD_REQUEST,
            ));
        }
        if fake.failing(Fault::StateRead) {
            return Err(throughput_exceeded());
        }
    }

    let storage = fake.lock();
    Ok(if table == FRAGMENT_STATE_TABLE_NAME {
        storage.state.get(&blob(item, "hash")).cloned()
    } else if table == FRAGMENT_METADATA_TABLE_NAME {
        storage.legacy_metadata.get(&blob(item, "hash")).cloned()
    } else {
        storage
            .associations
            .get(&(
                blob(item, "hash"),
                blob(item, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
            ))
            .cloned()
    })
}

impl Fake {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Storage> {
        self.0.lock().unwrap()
    }

    /// Make `fault` fail from now on. Latched rather than one-shot, so a retrying caller sees a
    /// persistent failure rather than one that heals underneath it.
    pub(crate) fn fail(&self, fault: Fault) {
        self.lock().faults.insert(fault);
    }

    /// Make the next check of `fault` fail and every later one pass, so a retrying caller sees a
    /// failure that heals. Only reaches faults checked through [`Fake::failing`].
    pub(crate) fn fail_once(&self, fault: Fault) {
        self.lock().faults_once.insert(fault);
    }

    /// Move `hash` into `state` at the moment its payload is uploaded, so a put reaches its
    /// publish step having probed before an obliteration and uploaded after it. That window
    /// cannot be hit by ordering calls from the outside.
    pub(crate) fn obliterate_during_upload(&self, hash: Hash, state: FragmentState) {
        self.lock().race_state = Some((hash, state));
    }

    pub(crate) fn failing(&self, fault: Fault) -> bool {
        let mut storage = self.lock();
        storage.faults.contains(&fault) || storage.faults_once.remove(&fault)
    }

    pub(crate) fn object_reads(&self) -> usize {
        self.lock().object_reads
    }

    pub(crate) fn state_reads(&self) -> usize {
        self.lock().state_reads
    }

    pub(crate) fn object(&self, hash: Hash) -> Option<StoredObject> {
        self.lock()
            .objects
            .get(&hash.to_string().into_bytes())
            .cloned()
    }

    pub(crate) fn stored_fragment(&self, hash: Hash) -> Option<Fragment> {
        self.object(hash)
            .map(|(_, metadata)| from_object_metadata(&metadata).unwrap())
    }

    pub(crate) fn state_of(&self, hash: Hash) -> Option<FragmentState> {
        self.lock()
            .state
            .get(hash.data().as_slice())
            .map(|item| from_item::<FragmentStateEntry>(item.clone()).unwrap())
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
        let item = to_item(FragmentStateEntry::new(hash, state)).unwrap();
        self.lock().state.insert(hash.data().to_vec(), item);
    }

    /// Write a row in the shape used before fragments moved onto the object: no `state`, and a
    /// whole flattened fragment whose `flags` also carry the obliteration bits.
    pub(crate) fn set_fragment_metadata_row(&self, hash: Hash, fragment: Fragment) {
        let item = legacy_metadata_item(hash, fragment);

        self.lock().state.insert(hash.data().to_vec(), item);
    }

    /// Write a legacy fragment metadata row into the *separate* metadata table (keyed by
    /// `FRAGMENT_METADATA_TABLE_NAME`). Unlike `set_fragment_metadata_row`, this does NOT
    /// touch `storage.state`, so `load_state` returns `None` for the same hash, letting tests
    /// exercise the `do_query` branch that falls back to the metadata table when there is no
    /// state row.
    pub(crate) fn set_legacy_metadata_row(&self, hash: Hash, fragment: Fragment) {
        let item = legacy_metadata_item(hash, fragment);

        self.lock()
            .legacy_metadata
            .insert(hash.data().to_vec(), item);
    }

    /// Delete an object and every version of it while leaving every reference to it in place, as
    /// an obliteration interrupted before its tombstone or an S3 durability event would.
    pub(crate) fn lose_object(&self, hash: Hash) {
        let key = hash.to_string();
        let mut storage = self.lock();
        storage.objects.remove(key.as_bytes());
        storage
            .versions
            .retain(|version| version.key.as_str() != key);
    }

    /// Store an object the way one was stored before the fragment moved onto it: bare bytes,
    /// no fragment metadata.
    pub(crate) fn put_object_without_metadata(&self, hash: Hash, body: &[u8]) {
        self.lock()
            .put(&hash.to_string(), (body.to_vec(), ObjectMetadata::new()));
    }

    /// List another version of `key`, as a versioned bucket holds alongside whatever is stored
    /// there. A delete marker is listed and deletable like any version, and stores no bytes.
    pub(crate) fn add_version(&self, key: &str, version_id: &'static str, is_delete_marker: bool) {
        self.lock().versions.push(ObjectVersion {
            key: ByteStr::copy_from_str(key),
            version_id: Some(ByteStr::from_static(version_id)),
            is_delete_marker,
            is_latest: false,
        });
    }

    /// Every version listed under exactly `key`, delete markers included.
    pub(crate) fn versions(&self, key: &str) -> Vec<ObjectVersion> {
        self.lock()
            .versions
            .iter()
            .filter(|version| version.key.as_str() == key)
            .cloned()
            .collect()
    }

    /// Store an object that `get_object` will return exactly once before removing it.
    /// Not visible to `head_object`, so `head_fragment` returns 404 while `load` still succeeds.
    pub(crate) fn put_object_once(&self, hash: Hash, body: &[u8]) {
        self.lock().objects_once.insert(
            hash.to_string().into_bytes(),
            (body.to_vec(), ObjectMetadata::new()),
        );
    }

    /// Store an object with the `lore-fragment` metadata header encoded from `fragment`.
    pub(crate) fn put_object_with_fragment_metadata(
        &self,
        hash: Hash,
        body: &[u8],
        fragment: Fragment,
    ) {
        self.lock().put(
            &hash.to_string(),
            (
                body.to_vec(),
                lore_aws::store::object_metadata::to_object_metadata(&fragment),
            ),
        );
    }

    /// Store an object whose metadata is present but unreadable.
    pub(crate) fn put_object_with_damaged_metadata(&self, hash: Hash, body: &[u8]) {
        let mut metadata = ObjectMetadata::new();
        metadata.insert(
            ByteStr::from_static("lore-fragment"),
            ByteStr::from_static("not:a:fragment"),
        );

        self.lock()
            .put(&hash.to_string(), (body.to_vec(), metadata));
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
        let item: Item = to_item(&entry).unwrap();
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
    let mut s3 = MockS3Impl::default();
    let mut dynamodb = MockDynamoDb::default();

    let f = fake.clone();
    s3.expect_put_object()
        .returning(move |_, key, body, metadata| {
            let mut storage = f.lock();
            if storage.faults.contains(&Fault::ObjectWriteShed) {
                return Err(s3_error(
                    S3ErrorCode::SlowDown,
                    StatusCode::SERVICE_UNAVAILABLE,
                ));
            }
            if storage.faults.contains(&Fault::ObjectWriteDenied) {
                return Err(s3_error(S3ErrorCode::AccessDenied, StatusCode::FORBIDDEN));
            }

            storage.put(key, (body.to_vec(), metadata.unwrap_or_default()));

            if let Some((hash, state)) = storage.race_state.take() {
                let item = to_item(FragmentStateEntry::new(hash, state)).unwrap();
                storage.state.insert(hash.data().to_vec(), item);
            }

            Ok(S3WriteOutput::default())
        });

    let f = fake.clone();
    s3.expect_get_object().returning(move |_, key, _| {
        let mut storage = f.lock();
        storage.object_reads += 1;
        // Read here rather than through `failing`, which would take the lock this closure holds.
        let truncated = storage.faults.contains(&Fault::ObjectReadTruncated);
        let chunked = storage.faults.contains(&Fault::ObjectReadChunked);
        if storage.faults.contains(&Fault::ObjectReadDenied) {
            return Err(s3_error(S3ErrorCode::AccessDenied, StatusCode::FORBIDDEN));
        }

        if let Some((body, metadata)) = storage.objects_once.remove(key.as_bytes()) {
            let content_length = Some(body.len() as u64);

            return Ok(GetObjectOutput {
                metadata,
                content_length,
                body: ByteStream::from_bytes(Bytes::from(body)),
                request_id: None,
            });
        }

        match storage.objects.get(key.as_bytes()) {
            Some((body, metadata)) => Ok(GetObjectOutput {
                metadata: metadata.clone(),
                // The claimed length is the whole object either way; a truncated body is one that
                // stops short of what its own header promised. A chunked reply declares none at all.
                content_length: (!chunked).then_some(body.len() as u64),
                body: if truncated {
                    ByteStream::truncated(Bytes::copy_from_slice(&body[..body.len() / 2]))
                } else if chunked {
                    ByteStream::chunked(Bytes::from(body.clone()), CHUNK_SIZE)
                } else {
                    ByteStream::from_bytes(Bytes::from(body.clone()))
                },
                request_id: None,
            }),
            None => Err(s3_error(S3ErrorCode::NoSuchKey, StatusCode::NOT_FOUND)),
        }
    });

    let f = fake.clone();
    s3.expect_head_object().returning(move |_, key| {
        let mut storage = f.lock();
        storage.object_reads += 1;
        if storage.faults.contains(&Fault::ObjectHeadDenied) {
            return Err(s3_error(S3ErrorCode::AccessDenied, StatusCode::FORBIDDEN));
        }

        match storage.objects.get(key.as_bytes()) {
            Some((body, metadata)) => Ok(HeadObjectOutput {
                metadata: metadata.clone(),
                content_length: Some(body.len() as u64),
                request_id: None,
            }),
            None => Err(s3_error(S3ErrorCode::NotFound, StatusCode::NOT_FOUND)),
        }
    });

    // Deleting a version removes it from the listing, a delete marker included. Naming no version
    // deletes the key's `null` version, as a bucket without versioning does. The bytes go once no
    // stored version of the key is left.
    let f = fake.clone();
    s3.expect_delete_object().returning(move |_, key, version| {
        if f.failing(Fault::ObjectDelete) {
            return Err(s3_error(
                S3ErrorCode::InternalError,
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }

        let version = version.unwrap_or(NULL_VERSION);
        let mut storage = f.lock();
        storage.versions.retain(|listed| {
            !(listed.key.as_str() == key && listed.version_id.as_deref() == Some(version))
        });
        if !storage
            .versions
            .iter()
            .any(|listed| listed.key.as_str() == key && !listed.is_delete_marker)
        {
            storage.objects.remove(key.as_bytes());
        }

        Ok(S3WriteOutput::default())
    });

    // A listing is by prefix, so it names every key that begins with the one asked for.
    let f = fake.clone();
    s3.expect_list_versions().returning(move |_, prefix| {
        if f.failing(Fault::ObjectList) {
            return Err(s3_error(
                S3ErrorCode::InternalError,
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }

        let storage = f.lock();
        Ok(ListObjectVersionsOutput {
            versions: storage
                .versions
                .iter()
                .filter(|version| version.key.as_str().starts_with(prefix))
                .cloned()
                .collect(),
            truncated: storage.faults.contains(&Fault::ObjectListTruncated),
            ..ListObjectVersionsOutput::default()
        })
    });

    let f = fake.clone();
    dynamodb.expect_get_item().returning(move |table, item, _| {
        Ok(GetItemOutput {
            item: read_one(&f, table, &item)?,
            request_id: None,
        })
    });

    // `item_exists` is the same request as `get_item` — only the response parsing differs — so the
    // fake answers it from the same read, and injects the same faults.
    let f = fake.clone();
    dynamodb
        .expect_item_exists()
        .returning(move |table, item, _| Ok(read_one(&f, table, &item)?.is_some()));

    let f = fake.clone();
    dynamodb
        .expect_batch_get_item()
        .returning(move |table, keys, _| {
            if f.failing(Fault::BatchReadUnprocessed) {
                return Err(AwsError::UnprocessedKeys {
                    remaining: keys.len(),
                });
            }

            // The service refuses a request naming one key twice.
            let mut distinct = HashSet::with_capacity(keys.len());
            if !keys.iter().all(|key| {
                distinct.insert((
                    blob(key, "hash"),
                    blob(key, FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
                ))
            }) {
                return Err(dynamo_error(
                    DynamoErrorCode::ValidationException,
                    StatusCode::BAD_REQUEST,
                ));
            }

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
            return Err(throughput_exceeded());
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
        Ok(WriteOutput::default())
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
            Ok(WriteOutput::default())
        });

    let f = fake.clone();
    dynamodb
        .expect_put_item_conditional::<StateUnchanged>()
        .returning(move |_, item, condition| {
            let updated = from_item::<FragmentStateEntry>(item.clone())
                .unwrap()
                .state();
            let releasing =
                condition.0 == FragmentState::Obliterating && updated == FragmentState::Stored;
            let finalizing =
                condition.0 == FragmentState::Obliterating && updated == FragmentState::Obliterated;
            if f.failing(Fault::StateWrite)
                || (releasing && f.failing(Fault::StateRelease))
                || (finalizing && f.failing(Fault::StateFinalize))
            {
                return Err(throughput_exceeded());
            }

            let mut storage = f.lock();
            let key = blob(&item, "hash");

            let current = storage.state.get(&key).map(|existing| {
                from_item::<FragmentStateEntry>(existing.clone())
                    .unwrap()
                    .state()
            });

            if current == Some(condition.0) {
                storage.state.insert(key, item);
                Ok(WriteOutput::default())
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
            return Err(throughput_exceeded());
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
        Ok(WriteOutput::default())
    });

    let f = fake.clone();
    dynamodb.expect_query_single().returning(move |_, query| {
        if f.failing(Fault::AssociationCount) {
            return Err(throughput_exceeded());
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
        Ok(QueryOutput {
            count: i32::from(matched),
            ..QueryOutput::default()
        })
    });

    let f = fake.clone();
    dynamodb.expect_query_single().returning(move |_, query| {
        if f.failing(Fault::AssociationCount) {
            return Err(throughput_exceeded());
        }

        let storage = f.lock();
        let PartitionAssociationQuery(hash, partition) = query;
        // The service's `begins_with` on a sort key of partition followed by context.
        let matched = storage.associations.keys().any(|(stored, sort_key)| {
            stored == hash.data() && sort_key.starts_with(partition.data())
        });

        Ok(QueryOutput {
            count: i32::from(matched),
            ..QueryOutput::default()
        })
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

    let execution = crate::store::setup_execution("test".to_string());
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

    let execution = crate::store::setup_execution("test".to_string());
    LORE_CONTEXT
        .scope(execution, async move {
            Arc::new(AwsImmutableStore::new(s3, dynamodb, &settings))
        })
        .await
}

/// A service error, as either client reports one.
fn service_error<E: ServiceErrorCode>(
    code: E,
    status: StatusCode,
    payload: ErrorPayload,
) -> AwsError<AwsCallError<E>> {
    AwsError::Aws(AwsCallError::service(ServiceError {
        status,
        code,
        message: None,
        request_id: None,
        payload,
    }))
}

pub(crate) fn dynamo_error(code: DynamoErrorCode, status: StatusCode) -> DynamoError {
    service_error(code, status, ErrorPayload::None)
}

pub(crate) fn s3_error(code: S3ErrorCode, status: StatusCode) -> S3Error {
    service_error(code, status, ErrorPayload::None)
}

pub(crate) fn blob(item: &Item, key: &str) -> Vec<u8> {
    item.get(key)
        .and_then(AttributeValue::as_blob)
        .map_or_default(<[u8]>::to_vec)
}

/// The row shape used before fragments moved onto the object: no `state`, and a flattened fragment.
fn legacy_metadata_item(hash: Hash, fragment: Fragment) -> Item {
    let mut item = Item::new();
    item.insert(
        ByteStr::from_static("hash"),
        AttributeValue::blob(hash.data()),
    );
    item.insert(
        ByteStr::from_static("flags"),
        AttributeValue::number(fragment.flags),
    );
    item.insert(
        ByteStr::from_static("size_payload"),
        AttributeValue::number(fragment.size_payload),
    );
    item.insert(
        ByteStr::from_static("size_content"),
        AttributeValue::number(fragment.size_content),
    );
    item
}

pub(crate) fn throughput_exceeded() -> DynamoError {
    dynamo_error(
        DynamoErrorCode::ProvisionedThroughputExceededException,
        StatusCode::BAD_REQUEST,
    )
}

pub(crate) fn conditional_check_failed(item: Item) -> DynamoError {
    service_error(
        DynamoErrorCode::ConditionalCheckFailedException,
        StatusCode::BAD_REQUEST,
        ErrorPayload::Item(Box::new(item)),
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
