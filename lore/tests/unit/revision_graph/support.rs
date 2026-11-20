// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared fixtures for the revision graph verbs' tests: an in-memory storage handle opened as a
//! caller opens one, an execution context for the setup a test writes through the stores,
//! branches, metadata blobs, lines of revisions and latest revisions written through a context, a
//! branch created and archived in a handle's local store, a handle's state over other stores, a
//! handle whose mutable store fails chosen calls, an immutable store that counts its reads, a
//! remote revision service whose answers a test chooses, a metadata blob held by a remote storage
//! peer alone, a registered instance that reads as live, and the record and location complete
//! events a call sends.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::storage::close::LoreStorageCloseArgs;
use lore::storage::close::close;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::open;
use lore::storage::store::BoundFlags;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::Disconnected;
use lore_base::error::NotFound;
use lore_base::error::SlowDown;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreMetadataEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchMetadataEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphLocationCompleteEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphRevisionEventData;
use lore_revision::immutable;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreMetadata;
use lore_revision::metadata::Metadata;
use lore_revision::protocol::RemoteConnections;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_revision::test_util::write_state_revision;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::local::store_lock::StoreHold;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::ImmutableCall;
use lore_storage::test_util::StoreCall;
use lore_transport::BranchListResponse;
use lore_transport::BranchPushError;
use lore_transport::BranchPushResponse;
use lore_transport::BranchQueryResponse;
use lore_transport::BranchRecord;
use lore_transport::Connection;
use lore_transport::MetadataSetResult;
use lore_transport::ProtocolError;
use lore_transport::Revision;
use lore_transport::RevisionListResponse;
use lore_transport::RevisionListStart;
use lore_transport::Storage;
use lore_transport::test_util::MemoryPeer;

use crate::call::test_support::opened_handle_id;
use crate::call::test_support::recording_callback;
use crate::storage::in_memory_store;
use crate::storage::under_globals;

/// The category of every branch [`create`] creates.
pub(crate) const CATEGORY: &str = "tools";
/// The time every branch [`create`] creates was created, in milliseconds since the Unix epoch.
pub(crate) const CREATED: u64 = 1_762_905_600_000;

/// A new branch id.
pub(crate) fn new_branch() -> BranchId {
    BranchId::from(rand::random::<[u8; 16]>())
}

/// A new repository id.
pub(crate) fn new_repository() -> Partition {
    Partition::from(rand::random::<[u8; 16]>())
}

/// The hash whose 32 bytes are each `byte`.
pub(crate) fn hash(byte: u8) -> Hash {
    Hash::from([byte; 32])
}

/// A new in-memory store, and a handle to it from `lore::storage::open::open`.
pub(crate) async fn open_in_memory_handle() -> (Arc<StoreInternal>, LoreStore) {
    let (events, callback) = recording_callback();
    let args = LoreStorageOpenArgs {
        in_memory: 1,
        ..Default::default()
    };
    assert_eq!(open(LoreGlobalArgs::default(), args, callback).await, 0);
    let handle_id = opened_handle_id(&events.lock().unwrap()).expect("the open reports a handle");
    let store_handle = LoreStore { handle_id };
    let store = handle::lookup(store_handle).expect("the handle is registered");
    (store, store_handle)
}

/// Runs `body`, setup that writes through a handle's stores, under an execution context of its
/// own that sends no events.
pub(crate) async fn in_setup_context<T>(body: impl Future<Output = T>) -> T {
    under_globals(LoreGlobalArgs::default(), body).await
}

/// Registers a new instance of `repository` in `store` at `dir`, as
/// `lore_revision::test_util::register_live_instance` does. Returns the instance's id.
pub(crate) async fn register_live_instance(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    revision: Hash,
    dir: &TempDir,
) -> InstanceId {
    lore_revision::test_util::register_live_instance(
        store.immutable.clone(),
        store.mutable.clone(),
        repository,
        branch,
        revision,
        dir.path(),
    )
    .await
}

/// Closes `store`, a handle a test opened.
pub(crate) async fn close_handle(store: LoreStore) {
    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "closes the handle");
}

/// Creates `branch`, named `name` and created by `creator`, in the local store through `write`,
/// with `stack`, at `latest` with `status`, with [`CATEGORY`] and [`CREATED`].
pub(crate) async fn create(
    write: &Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    creator: &str,
    stack: Vec<BranchPoint>,
    latest: Hash,
    status: BranchLatestStatus,
) {
    branch::create_record(
        write.clone(),
        branch,
        name,
        CATEGORY,
        creator,
        CREATED,
        stack,
        latest,
        status,
        Context::default(),
    )
    .await
    .unwrap_or_else(|err| panic!("creating {name}: {err}"));
}

/// Creates `branch`, named `name` and created by `creator`, in the local store of `store`, at
/// `latest`, with one branch point.
pub(crate) async fn create_in_store(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    name: &str,
    creator: &str,
    latest: Hash,
) {
    let stack = vec![BranchPoint {
        branch: new_branch(),
        revision: hash(1),
    }];
    let write = local_write_context(store, repository);
    in_setup_context(create(
        &write,
        branch,
        name,
        creator,
        stack,
        latest,
        BranchLatestStatus::Convergent,
    ))
    .await;
}

/// Archives `branch`, named `name`, in the local store of `store`, writing what an archive writes.
pub(crate) async fn archive(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    name: &str,
) {
    let write = local_write_context(store, repository);
    in_setup_context(async {
        let expected = branch::mutable_load(write.clone(), branch::METADATA, branch)
            .await
            .expect("loads the metadata key");
        let metadata = branch::load_metadata(write.clone(), expected)
            .await
            .expect("reads the metadata");
        branch::archive_record(write.clone(), branch, name, expected, metadata)
            .await
            .unwrap_or_else(|err| panic!("archiving {name}: {err}"));
    })
    .await;
}

/// Writes through `write` a revision metadata blob that names `branch`, and returns its hash.
pub(crate) async fn metadata_naming(write: &Arc<RepositoryContext>, branch: BranchId) -> Hash {
    let mut metadata = Metadata::new();
    metadata.set_branch(branch).expect("sets the branch");
    metadata
        .serialize(write.clone())
        .await
        .expect("writes the metadata")
}

/// Writes through `write` a line of `count` revisions numbered from 1, each with metadata naming
/// `branch`, and returns them oldest first.
pub(crate) async fn write_line(
    write: &Arc<RepositoryContext>,
    branch: BranchId,
    count: u64,
) -> Vec<Hash> {
    let metadata = metadata_naming(write, branch).await;
    let mut line = Vec::new();
    let mut parent = Hash::default();
    for number in 1..=count {
        parent = write_state_revision(write, parent, number, metadata).await;
        line.push(parent);
    }
    line
}

/// Stores `latest` as the latest revision of `branch` in the local store of `store`, with no
/// metadata or name.
pub(crate) async fn store_latest(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    latest: Hash,
) {
    let write = local_write_context(store, repository);
    in_setup_context(branch::mutable_try_store(
        write,
        branch::LATEST,
        branch,
        Hash::default(),
        latest,
    ))
    .await
    .expect("stores the latest revision");
}

/// Writes repository metadata naming `default_branch` the default branch through `write`, and
/// points the repository's metadata key at it.
pub(crate) async fn store_repository_metadata(
    write: &Arc<RepositoryContext>,
    default_branch: BranchId,
) {
    let hash = repository::metadata_store(
        write.clone(),
        RepositoryMetadata {
            name: "graph".to_owned(),
            default_branch,
            default_branch_name: "main".to_owned(),
            ..Default::default()
        },
    )
    .await
    .expect("writes the repository metadata");
    repository::metadata_store_hash(write.clone(), hash)
        .await
        .expect("stores the repository metadata key");
}

/// Metadata holding `id`, where given, and the string `entries`.
pub(crate) fn partial_metadata(id: Option<BranchId>, entries: &[(&str, &str)]) -> Metadata {
    let mut metadata = Metadata::new();
    if let Some(id) = id {
        metadata
            .set_binary(branch::ID, id.data())
            .expect("sets the id");
    }
    for (key, value) in entries {
        metadata.set_string(key, value).expect("sets an entry");
    }
    metadata
}

/// Writes `metadata` as a blob through `write` and points the metadata key of `branch` at it.
pub(crate) async fn store_metadata(
    write: &Arc<RepositoryContext>,
    branch: BranchId,
    metadata: Metadata,
) {
    let blob = metadata
        .serialize(write.clone())
        .await
        .expect("writes the blob");
    branch::mutable_store_metadata(write.clone(), branch, blob)
        .await
        .expect("stores the metadata key");
}

/// Writes `bytes` as a blob through `write` and points the metadata key of `branch` at it. Returns
/// the blob's address.
pub(crate) async fn store_bytes_as_metadata(
    write: &Arc<RepositoryContext>,
    branch: BranchId,
    bytes: &[u8],
) -> Address {
    let address = immutable::write_borrowed(
        write.clone(),
        Context::default(),
        bytes,
        immutable::write_options_from_repository(write.clone()),
    )
    .await
    .expect("writes the bytes");
    branch::mutable_store_metadata(write.clone(), branch, address.hash)
        .await
        .expect("stores the metadata key");
    address
}

/// Writes bytes that aren't a metadata blob through `write` and points the metadata key of
/// `branch` at them.
pub(crate) async fn store_corrupt_metadata(write: &Arc<RepositoryContext>, branch: BranchId) {
    store_bytes_as_metadata(write, branch, b"not metadata").await;
}

/// The metadata of `branch` in the local store of `store`.
pub(crate) async fn local_metadata(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
) -> Metadata {
    in_setup_context(branch::local_metadata(
        local_read_context(store, repository),
        branch,
    ))
    .await
    .expect("reads the metadata")
    .expect("the branch has metadata")
    .1
}

/// Puts `metadata`, as a blob of one fragment (256 KiB at most), into `peer` alone: written
/// through a scratch store of its own, so no store a test reads holds it. Returns the blob's hash.
pub(crate) async fn put_metadata_blob(peer: &Arc<MemoryPeer>, metadata: &Metadata) -> Hash {
    let scratch = in_memory_for_tests("graph-remote-metadata-blob").await;
    let repository = new_repository();
    let write = local_write_context(&scratch, repository);
    in_setup_context(async {
        let hash = metadata
            .serialize(write.clone())
            .await
            .expect("writes the blob");
        let address = Address {
            hash,
            context: Context::default(),
        };
        let (fragment, payload) =
            immutable::load_raw_store_retry(scratch.immutable.clone(), repository, address)
                .await
                .expect("loads the blob");
        peer.put(0, address, fragment, Some(payload))
            .await
            .expect("the peer keeps the blob");
        hash
    })
    .await
}

/// Puts into `peer` the root fragment of the blob at `address` in the local store of `store`, with
/// its payload: for a blob over 256 KiB, its fragment list alone.
pub(crate) async fn put_root_fragment(
    peer: &Arc<MemoryPeer>,
    store: &StoreInternal,
    repository: Partition,
    address: Address,
) {
    let (fragment, payload) = in_setup_context(immutable::load_raw_store_retry(
        store.immutable.clone(),
        repository,
        address,
    ))
    .await
    .expect("loads the root fragment");
    peer.put(0, address, fragment, Some(payload))
        .await
        .expect("the peer keeps the root fragment");
}

/// The state of a new handle over `immutable` and `mutable`: it keeps the identity, the remote, the
/// bound flags and the trust of `store`, and holds no claim on the stores.
pub(crate) fn with_stores(
    store: &StoreInternal,
    immutable: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
) -> StoreInternal {
    StoreInternal::new(
        store.identity.clone(),
        immutable,
        mutable,
        store.remote.clone(),
        store.bound_flags,
        store.trust_remote,
        StoreHold::default(),
    )
}

/// A handle over [`failing_store`] of `store` and `fails`.
pub(crate) fn handle_failing(
    store: &StoreInternal,
    fails: impl Fn(StoreCall, Hash, KeyType) -> bool + Send + Sync + 'static,
) -> LoreStore {
    handle::register(Arc::new(failing_store(store, fails)))
}

/// [`with_mutable_store`] over the mutable store of `store`, failing each call `fails` picks with
/// `Disconnected`, given the call, the key and the key type, a zero key for a list.
pub(crate) fn failing_store(
    store: &StoreInternal,
    fails: impl Fn(StoreCall, Hash, KeyType) -> bool + Send + Sync + 'static,
) -> StoreInternal {
    let hooked = HookedStore::wrap(
        store.mutable.clone(),
        move |call, key, key_type| {
            if fails(call, key, key_type) {
                return Err(StoreError::from(Disconnected));
            }
            Ok(())
        },
        None,
    );
    with_mutable_store(store, hooked)
}

/// [`with_stores`] over the immutable store of `store` and `mutable`.
pub(crate) fn with_mutable_store(
    store: &StoreInternal,
    mutable: Arc<dyn MutableStore>,
) -> StoreInternal {
    with_stores(store, store.immutable.clone(), mutable)
}

/// The payload reads of each hash that reach an immutable store, as [`counting_reads`] counts
/// them.
#[derive(Default)]
pub(crate) struct Reads(Mutex<HashMap<Hash, usize>>);

impl Reads {
    /// The payload reads of `hash` counted so far.
    pub(crate) fn of(&self, hash: Hash) -> usize {
        self.0
            .lock()
            .unwrap()
            .get(&hash)
            .copied()
            .unwrap_or_default()
    }
}

/// `inner`, counting the payload reads of each hash that reach it in the returned [`Reads`].
pub(crate) fn counting_reads(
    inner: Arc<dyn ImmutableStore>,
) -> (Arc<dyn ImmutableStore>, Arc<Reads>) {
    let reads = Arc::new(Reads::default());
    let counted = reads.clone();
    let store = HookedImmutableStore::wrap(inner, move |call, _, address| {
        if call == ImmutableCall::Get {
            *counted.0.lock().unwrap().entry(address.hash).or_default() += 1;
        }
        Ok(())
    });
    (store, reads)
}

/// How [`NamingRevision`] answers a branch query.
#[derive(Clone)]
pub(crate) enum QueryAnswer {
    /// The record of a live branch with this id, named `feature`, without a latest revision.
    Record(BranchId),
    /// This record.
    Answer(BranchQueryResponse),
    /// No live branch holds the name or the id.
    NotFound,
    /// The remote is overloaded.
    SlowDown,
}

/// How [`NamingRevision`] answers a revision list.
#[derive(Clone, Copy)]
pub(crate) enum ListAnswer {
    /// The service doesn't serve it.
    NotServed,
    /// Lore Server's answer for a branch it doesn't hold and for a number it can't resolve.
    NotFound,
    /// The remote is overloaded.
    SlowDown,
}

/// How [`NamingRevision`] answers a branch metadata get.
pub(crate) enum MetadataAnswer {
    /// The service doesn't serve it.
    NotServed,
    /// The branch's metadata hash, zero for a branch without metadata.
    Hash(Hash),
    /// No branch holds the id.
    NotFound,
    /// The request fails with this error.
    Fails(ProtocolError),
}

/// How [`NamingRevision`] answers a branch metadata set.
pub(crate) enum SetAnswer {
    /// The swap stores the new metadata.
    Won,
    /// Another writer stored this hash first, which the swap answers.
    Lost(Hash),
    /// The branch's metadata went before the set: the set answers `NotFound`, and so does every
    /// later metadata get.
    NotFound,
    /// The set fails with this error.
    Refused(ProtocolError),
}

/// A remote revision service that answers every branch query as `query` says, every revision list
/// as `list` says, every branch metadata get as `metadata` says and each branch metadata set as the
/// next of its set answers says, records what each branch query, metadata get and metadata set
/// asked for, and serves no other request.
pub(crate) struct NamingRevision {
    query: QueryAnswer,
    list: ListAnswer,
    metadata: MetadataAnswer,
    /// The id and the name each branch query asked for, in order.
    pub(crate) queries: Mutex<Vec<(Option<BranchId>, Option<String>)>>,
    /// The branch each metadata get asked for, in order.
    pub(crate) metadata_gets: Mutex<Vec<BranchId>>,
    /// The answers to the metadata sets still to come, in order; none left refuses a set.
    set_answers: Mutex<VecDeque<SetAnswer>>,
    /// The expected and the updated hash each metadata set asked for, in order.
    pub(crate) metadata_sets: Mutex<Vec<(Hash, Hash)>>,
    /// Whether a set answered [`SetAnswer::NotFound`].
    metadata_went: AtomicBool,
}

impl NamingRevision {
    /// A service that answers branch queries as `query` says, and serves no revision list.
    pub(crate) fn new(query: QueryAnswer) -> Arc<Self> {
        Self::listing(query, ListAnswer::NotServed)
    }

    /// A service that answers branch queries as `query` says, and revision lists as `list` says.
    pub(crate) fn listing(query: QueryAnswer, list: ListAnswer) -> Arc<Self> {
        Self::answering(query, list, MetadataAnswer::NotServed)
    }

    /// A service that answers branch metadata gets as `metadata` says, every branch query
    /// `NotFound`, and serves no revision list.
    pub(crate) fn serving_metadata(metadata: MetadataAnswer) -> Arc<Self> {
        Self::answering(QueryAnswer::NotFound, ListAnswer::NotServed, metadata)
    }

    /// A service that answers branch metadata gets as `metadata` says and its branch metadata sets
    /// with `sets` in order, every branch query `NotFound`, and serves no revision list.
    pub(crate) fn setting_metadata(
        metadata: MetadataAnswer,
        sets: impl IntoIterator<Item = SetAnswer>,
    ) -> Arc<Self> {
        let service = Self::serving_metadata(metadata);
        service.set_answers.lock().unwrap().extend(sets);
        service
    }

    fn answering(query: QueryAnswer, list: ListAnswer, metadata: MetadataAnswer) -> Arc<Self> {
        Arc::new(Self {
            query,
            list,
            metadata,
            queries: Mutex::default(),
            metadata_gets: Mutex::default(),
            set_answers: Mutex::default(),
            metadata_sets: Mutex::default(),
            metadata_went: AtomicBool::new(false),
        })
    }
}

/// The refusal of every request [`NamingRevision`] doesn't serve.
fn not_served() -> ProtocolError {
    ProtocolError::internal("the test's revision service doesn't serve this request")
}

#[async_trait]
impl Revision for NamingRevision {
    async fn branch_create(
        &self,
        _: BranchId,
        _: &str,
        _: &str,
        _: &str,
        _: &[BranchPoint],
    ) -> Result<BranchRecord, ProtocolError> {
        Err(not_served())
    }

    async fn branch_delete(&self, _: BranchId) -> Result<(), ProtocolError> {
        Err(not_served())
    }

    async fn branch_query(
        &self,
        branch: Option<BranchId>,
        name: Option<&str>,
    ) -> Result<BranchQueryResponse, ProtocolError> {
        self.queries
            .lock()
            .unwrap()
            .push((branch, name.map(str::to_owned)));
        match &self.query {
            QueryAnswer::Record(id) => Ok(BranchQueryResponse {
                id: *id,
                name: "feature".to_owned(),
                metadata: hash(7),
                ..Default::default()
            }),
            QueryAnswer::Answer(record) => Ok(record.clone()),
            QueryAnswer::NotFound => Err(ProtocolError::from(NotFound)),
            QueryAnswer::SlowDown => Err(ProtocolError::from(SlowDown)),
        }
    }

    async fn branch_push(
        &self,
        _: BranchId,
        _: Hash,
        _: bool,
        _: bool,
    ) -> Result<BranchPushResponse, BranchPushError> {
        Err(not_served().into())
    }

    async fn branch_list(
        &self,
        _: Option<&str>,
        _: bool,
    ) -> Result<BranchListResponse, ProtocolError> {
        Err(not_served())
    }

    async fn revision_list(
        &self,
        _: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError> {
        Err(match self.list {
            ListAnswer::NotServed => not_served(),
            ListAnswer::NotFound => ProtocolError::from(NotFound),
            ListAnswer::SlowDown => ProtocolError::from(SlowDown),
        })
    }

    async fn branch_metadata_get(&self, branch: BranchId) -> Result<Hash, ProtocolError> {
        self.metadata_gets.lock().unwrap().push(branch);
        if self.metadata_went.load(Ordering::SeqCst) {
            return Err(ProtocolError::from(NotFound));
        }
        match &self.metadata {
            MetadataAnswer::NotServed => Err(not_served()),
            MetadataAnswer::Hash(hash) => Ok(*hash),
            MetadataAnswer::NotFound => Err(ProtocolError::from(NotFound)),
            MetadataAnswer::Fails(err) => Err(err.clone()),
        }
    }

    async fn branch_metadata_set(
        &self,
        _: BranchId,
        expected: Hash,
        updated: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        self.metadata_sets.lock().unwrap().push((expected, updated));
        let Some(answer) = self.set_answers.lock().unwrap().pop_front() else {
            return Err(not_served());
        };
        match answer {
            SetAnswer::Won => Ok(MetadataSetResult {
                success: true,
                current_hash: updated,
            }),
            SetAnswer::Lost(current_hash) => Ok(MetadataSetResult {
                success: false,
                current_hash,
            }),
            SetAnswer::NotFound => {
                self.metadata_went.store(true, Ordering::SeqCst);
                Err(ProtocolError::from(NotFound))
            }
            SetAnswer::Refused(err) => Err(err),
        }
    }
}

/// A new in-memory store whose remote reaches `service` as its revision service for `repository`,
/// over a connection with no storage.
pub(crate) async fn store_with_remote(
    repository: Partition,
    service: Arc<NamingRevision>,
) -> StoreInternal {
    store_with_remote_storage(repository, service, Vec::new()).await
}

/// A new in-memory store whose remote reaches `service` as its revision service for `repository`,
/// over a connection with no storage that authenticated as `identity`, and that holds no identity of
/// its own.
pub(crate) async fn store_with_remote_as(
    repository: Partition,
    service: Arc<dyn Revision>,
    identity: &str,
) -> StoreInternal {
    let connection = Connection::detached_revision_as(
        identity.to_owned(),
        Vec::new(),
        Ok(()),
        repository,
        service,
    );
    let endpoint = RemoteEndpoint::over(RemoteConnections::with_connection(repository, connection));
    let store = in_memory_store(Some(Arc::new(endpoint)), BoundFlags::default()).await;
    StoreInternal::new(
        String::new(),
        store.immutable.clone(),
        store.mutable.clone(),
        store.remote.clone(),
        store.bound_flags,
        store.trust_remote,
        StoreHold::default(),
    )
}

/// A new in-memory store whose remote reaches `service` as its revision service for `repository`,
/// over a connection whose storage is `storage`.
pub(crate) async fn store_with_remote_storage(
    repository: Partition,
    service: Arc<NamingRevision>,
    storage: Vec<Arc<dyn Storage>>,
) -> StoreInternal {
    let connection = Connection::detached_revision(storage, Ok(()), repository, service);
    let endpoint = RemoteEndpoint::over(RemoteConnections::with_connection(repository, connection));
    in_memory_store(Some(Arc::new(endpoint)), BoundFlags::default()).await
}

/// The branch record events among `events`, in order.
pub(crate) fn branch_records(events: &[LoreEvent]) -> Vec<&LoreRevisionGraphBranchEventData> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranch(data) => Some(data),
            _ => None,
        })
        .collect()
}

/// A branch metadata record as a test compares it: its location, key and value.
pub(crate) type Reported = (LoreBranchLocation, String, LoreMetadata);

/// The branch metadata records among `events`, in order, each echoing `id`, the call's id, and
/// naming `branch`.
pub(crate) fn reported(events: &[LoreEvent], id: u64, branch: BranchId) -> Vec<Reported> {
    branch_metadata_records(events)
        .into_iter()
        .map(|record| {
            assert_eq!((record.id, record.branch), (id, branch));
            (
                record.location,
                record.key.as_str().to_owned(),
                record.value.clone(),
            )
        })
        .collect()
}

/// Each entry of `metadata` as a branch metadata record at `location` reports it, in stored order.
pub(crate) fn entries_at(location: LoreBranchLocation, metadata: &Metadata) -> Vec<Reported> {
    let mut entries = Vec::new();
    metadata.walk(|key, value, value_type| {
        let key = std::str::from_utf8(key).expect("a UTF-8 key");
        let entry =
            LoreMetadataEventData::new(key, value, value_type).expect("a value of its type");
        entries.push((location, key.to_owned(), entry.value));
    });
    entries
}

/// The branch metadata record events among `events`, in order.
pub(crate) fn branch_metadata_records(
    events: &[LoreEvent],
) -> Vec<&LoreRevisionGraphBranchMetadataEventData> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranchMetadata(data) => Some(data),
            _ => None,
        })
        .collect()
}

/// The revision record events among `events`, in order.
pub(crate) fn revision_records(events: &[LoreEvent]) -> Vec<&LoreRevisionGraphRevisionEventData> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphRevision(data) => Some(data),
            _ => None,
        })
        .collect()
}

/// The location complete events among `events` as (location, records, error code), in order, each
/// echoing the call's `id`.
pub(crate) fn location_outcomes(
    events: &[LoreEvent],
    id: u64,
) -> Vec<(LoreBranchLocation, u64, i32)> {
    location_completes(events)
        .into_iter()
        .map(|complete| {
            assert_eq!(
                complete.id, id,
                "a location complete event echoes the call's id"
            );
            (
                complete.location,
                complete.records,
                complete.error.error_code,
            )
        })
        .collect()
}

/// The location complete events among `events`, in order.
pub(crate) fn location_completes(
    events: &[LoreEvent],
) -> Vec<&LoreRevisionGraphLocationCompleteEventData> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphLocationComplete(data) => Some(data),
            _ => None,
        })
        .collect()
}
