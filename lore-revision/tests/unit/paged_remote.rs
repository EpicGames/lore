// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A remote revision service that answers revision lists with fixed pages, and a store that
//! counts the reads reaching it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::Disconnected;
use lore_base::error::NotFound;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::KeyType;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_transport::BranchListResponse;
use lore_transport::BranchPushResponse;
use lore_transport::BranchQueryResponse;
use lore_transport::BranchRecord;
use lore_transport::Connection;
use lore_transport::MetadataSetResult;
use lore_transport::ProtocolError;
use lore_transport::Revision;
use lore_transport::RevisionItem;
use lore_transport::RevisionListResponse;
use lore_transport::RevisionListStart;
use tokio::sync::Notify;
use zerocopy::IntoBytes;

use crate::helper::in_memory;
use crate::hooked_store::Answered;
use crate::hooked_store::HookedStore;
use crate::hooked_store::StoreCall;

/// What a page answers: a revision number on the remote's branch, or a revision signature.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Anchor {
    Number(u64),
    Signature(Hash),
}

struct Page {
    anchor: Anchor,
    items: Vec<(u64, Hash)>,
    older: Hash,
}

/// A remote holding one branch at `latest`, with its metadata at `metadata`, which answers a query
/// by any name with that branch. It answers a revision list with the page registered for its
/// anchor and counts the revision lists it is asked for. An anchor with no page is not found, a
/// list for an anchor registered as failing fails, and a list for an anchor registered as stalling
/// is never answered. A page item carries the state of a revision the remote holds, and no state
/// otherwise. A remote with a `refusal` answers every branch query with it. It records each name a
/// branch query asks for.
pub(crate) struct PagedRemote {
    branch: BranchId,
    latest: Hash,
    metadata: Hash,
    pages: Vec<Page>,
    failures: Vec<(Anchor, ProtocolError)>,
    stalls: Vec<Anchor>,
    /// Signalled when a list left unanswered starts.
    stalled: Notify,
    /// Signalled when a list left unanswered is dropped, which is the only way it ends.
    abandoned: Notify,
    held: Vec<RemoteRevision>,
    /// The metadata hash a page item carries for a revision, by signature; zero for any other.
    listed_metadata: Vec<(Hash, Hash)>,
    lists: AtomicUsize,
    refusal: Option<ProtocolError>,
    queried_names: std::sync::Mutex<Vec<String>>,
}

/// Signals a [`PagedRemote`]'s `abandoned` when the list left unanswered that holds it is dropped.
struct Abandoned<'a>(&'a Notify);

impl Drop for Abandoned<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// A revision, with the state a page item carries for it.
#[derive(Clone)]
pub(crate) struct RemoteRevision {
    pub signature: Hash,
    pub state: Bytes,
}

/// The revision numbered `number`, with `parent` as its self parent and `metadata` as its metadata
/// hash, written nowhere the walk reads. A revision written with the same three is this one.
pub(crate) async fn remote_revision(number: u64, parent: Hash, metadata: Hash) -> RemoteRevision {
    let scratch = crate::branch::null_repository().await;
    let token = scratch
        .try_write_token()
        .expect("a null context carries a write token");
    let state = lore_revision::state::State::new();
    state.set_parent_self(parent);
    state.set_revision_number(number);
    state.set_metadata_hash(metadata);
    let signature = state
        .serialize(scratch.clone(), token)
        .await
        .expect("serializing the revision state");
    let state = Bytes::copy_from_slice(state.state_data().as_bytes());
    assert_eq!(
        Hash::hash_buffer(&state),
        signature,
        "a revision's signature is the hash of its state"
    );
    RemoteRevision { signature, state }
}

/// `revision` in each way a page can list it without its own state: with no state, with its state
/// cut short, and with `other`'s state. Each comes with its name, and with what a remote holding
/// it carries for it, if anything.
pub(crate) fn without_its_own_state(
    revision: &RemoteRevision,
    other: &RemoteRevision,
) -> [(&'static str, Option<RemoteRevision>); 3] {
    [
        ("missing", None),
        (
            "cut short",
            Some(RemoteRevision {
                signature: revision.signature,
                state: revision.state.slice(..revision.state.len() / 2),
            }),
        ),
        (
            "another revision's",
            Some(RemoteRevision {
                signature: revision.signature,
                state: other.state.clone(),
            }),
        ),
    ]
}

/// A local revision numbered 4 above two revisions only the remote holds, numbered 3 and 2, which
/// the remote lists in one page asked for at 3, one of them without its own state.
pub(crate) struct AboveUnverified {
    /// Which of the two the page lists without its own state: the revision asked for, or the one
    /// below it.
    pub position: &'static str,
    /// How the page lists it, as [`without_its_own_state`] names it.
    pub kind: &'static str,
    pub local: Hash,
    pub client: Arc<RepositoryContext>,
    /// A context over the client's stores that can write to them.
    pub writer: Arc<RepositoryContext>,
}

/// [`AboveUnverified`] for each position and each way [`without_its_own_state`] names, its local
/// revision with `metadata` as its metadata hash. Another revision's state numbers the revision in
/// order on the line, so only a check of the state finds it.
pub(crate) async fn above_unverified(metadata: Hash) -> Vec<AboveUnverified> {
    let deeper = remote_revision(2, Hash::default(), Hash::from([0xA0; 32])).await;
    let below = remote_revision(3, deeper.signature, Hash::from([0xA0; 32])).await;
    let other_at_3 = remote_revision(3, Hash::default(), Hash::from([0xB0; 32])).await;
    let other_at_1 = remote_revision(1, Hash::default(), Hash::from([0xB0; 32])).await;
    let mut cases = Vec::new();
    for (position, unverified, verified, other) in [
        ("the revision asked for", &below, &deeper, &other_at_3),
        ("a revision below it", &deeper, &below, &other_at_1),
    ] {
        for (kind, listed) in without_its_own_state(unverified, other) {
            let stores = client_stores().await;
            let state = lore_revision::state::State::new();
            state.set_parent_self(below.signature);
            state.set_revision_number(4);
            state.set_metadata_hash(metadata);
            let token = stores
                .writer
                .try_write_token()
                .expect("a null context carries a write token");
            let local = state
                .serialize(stores.writer.clone(), token)
                .await
                .expect("serializing the local revision");
            let remote = PagedRemote::new(BranchId::from([7; 16]), local)
                .holding(listed.iter().chain([verified]))
                .page(
                    Anchor::Signature(below.signature),
                    vec![(3, below.signature), (2, deeper.signature)],
                    Hash::default(),
                );
            cases.push(AboveUnverified {
                position,
                kind,
                local,
                client: client_on(&stores, Arc::new(remote)),
                writer: stores.writer,
            });
        }
    }
    cases
}

impl PagedRemote {
    pub(crate) fn new(branch: BranchId, latest: Hash) -> Self {
        Self {
            branch,
            latest,
            metadata: Hash::default(),
            pages: Vec::new(),
            failures: Vec::new(),
            stalls: Vec::new(),
            stalled: Notify::new(),
            abandoned: Notify::new(),
            held: Vec::new(),
            listed_metadata: Vec::new(),
            lists: AtomicUsize::new(0),
            refusal: None,
            queried_names: std::sync::Mutex::default(),
        }
    }

    /// Hold the branch's metadata at `metadata`.
    pub(crate) fn metadata(mut self, metadata: Hash) -> Self {
        self.metadata = metadata;
        self
    }

    /// Answer every branch query with `refusal`.
    pub(crate) fn refusing(mut self, refusal: ProtocolError) -> Self {
        self.refusal = Some(refusal);
        self
    }

    /// Answer `anchor` with `items`, newest first, and `older` as the cursor to the page
    /// below.
    pub(crate) fn page(mut self, anchor: Anchor, items: Vec<(u64, Hash)>, older: Hash) -> Self {
        self.pages.push(Page {
            anchor,
            items,
            older,
        });
        self
    }

    /// Hold `revisions`, so a page item for one carries its state.
    pub(crate) fn holding<'a>(
        mut self,
        revisions: impl IntoIterator<Item = &'a RemoteRevision>,
    ) -> Self {
        self.held.extend(revisions.into_iter().cloned());
        self
    }

    /// List `revision` with `metadata` as its metadata hash.
    pub(crate) fn listing_metadata(mut self, revision: Hash, metadata: Hash) -> Self {
        self.listed_metadata.push((revision, metadata));
        self
    }

    /// Fail every list for `anchor` with `failure`.
    pub(crate) fn failing(mut self, anchor: Anchor, failure: ProtocolError) -> Self {
        self.failures.push((anchor, failure));
        self
    }

    /// Leave every list for `anchor` unanswered.
    pub(crate) fn stalling(mut self, anchor: Anchor) -> Self {
        self.stalls.push(anchor);
        self
    }

    pub(crate) fn lists(&self) -> usize {
        self.lists.load(Ordering::Relaxed)
    }

    /// The names the branch queries asked for, in order.
    pub(crate) fn queried_names(&self) -> Vec<String> {
        self.queried_names
            .lock()
            .expect("the queried names' lock")
            .clone()
    }

    /// Wait until a list left unanswered starts.
    pub(crate) async fn stalled(&self) {
        self.stalled.notified().await;
    }

    /// Wait until a list left unanswered is dropped.
    pub(crate) async fn abandoned(&self) {
        self.abandoned.notified().await;
    }
}

/// Whether the local immutable store of `repository` holds the state of each of `revisions`.
pub(crate) async fn stored_locally(
    repository: &Arc<RepositoryContext>,
    revisions: &[Hash],
) -> Vec<bool> {
    let mut stored = Vec::with_capacity(revisions.len());
    for &revision in revisions {
        let address = Address::zero_context_hash(revision);
        stored.push(lore_revision::immutable::is_stored_local(repository.clone(), address).await);
    }
    stored
}

/// The in-memory stores of a client.
pub(crate) struct ClientStores {
    immutable: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    /// A context over the stores that can write the revisions the client holds locally.
    pub writer: Arc<RepositoryContext>,
}

pub(crate) async fn client_stores() -> ClientStores {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    stores_over(immutable, mutable)
}

/// [`client_stores`] whose immutable store counts the reads reaching it.
pub(crate) async fn counted_client_stores() -> (ClientStores, Arc<CountingStore>) {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    let counting = Arc::new(CountingStore {
        inner: immutable,
        payloads: AtomicUsize::new(0),
        queries: AtomicUsize::new(0),
        failing: AtomicBool::new(false),
    });
    (stores_over(counting.clone(), mutable), counting)
}

/// [`client_stores`] whose mutable store runs `hook` before each load, store, compare-and-swap and
/// list it forwards.
pub(crate) async fn hooked_client_stores(
    hook: impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
) -> ClientStores {
    answering_client_stores(hook, None).await
}

/// [`hooked_client_stores`] whose mutable store also shows each load, store and compare-and-swap
/// it answers without an error to `answered`, if any ([`HookedStore`]), so a test can write
/// between a command's calls.
pub(crate) async fn answering_client_stores(
    hook: impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
    answered: Option<Answered>,
) -> ClientStores {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    stores_over(immutable, HookedStore::wrap(mutable, hook, answered))
}

impl ClientStores {
    /// A context over the stores, as `writer` is, whose local reads tell a failed read apart from
    /// a miss ([`RepositoryContext::strict_local_reads`]).
    pub(crate) fn strict_reader(&self) -> Arc<RepositoryContext> {
        Arc::new(
            RepositoryContext::new_null_context(self.immutable.clone(), self.mutable.clone())
                .strict_local_reads(),
        )
    }
}

fn stores_over(immutable: Arc<dyn ImmutableStore>, mutable: Arc<dyn MutableStore>) -> ClientStores {
    let writer = Arc::new(RepositoryContext::new_null_context(
        immutable.clone(),
        mutable.clone(),
    ));
    ClientStores {
        immutable,
        mutable,
        writer,
    }
}

/// The reads that reached a [`CountingStore`]'s store since they were last taken: the payloads
/// read, and the addresses queried.
#[derive(Debug, PartialEq)]
pub(crate) struct Reads {
    pub payloads: usize,
    pub queries: usize,
}

/// An immutable store that counts the reads reaching the store it wraps, and fails each payload
/// read after [`Self::fail_reads`] is called.
pub(crate) struct CountingStore {
    inner: Arc<dyn ImmutableStore>,
    payloads: AtomicUsize,
    queries: AtomicUsize,
    failing: AtomicBool,
}

impl CountingStore {
    /// The reads counted since the last call.
    pub(crate) fn take_reads(&self) -> Reads {
        Reads {
            payloads: self.payloads.swap(0, Ordering::Relaxed),
            queries: self.queries.swap(0, Ordering::Relaxed),
        }
    }

    /// Answer each payload read from now on with `Disconnected`, which no local read retries.
    pub(crate) fn fail_reads(&self) {
        self.failing.store(true, Ordering::Relaxed);
    }
}

#[async_trait]
impl ImmutableStore for CountingStore {
    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn isolates_partitions(&self) -> bool {
        self.inner.isolates_partitions()
    }

    fn read_scope(&self) -> StoreMatch {
        self.inner.read_scope()
    }

    fn query_scope(&self) -> StoreMatch {
        self.inner.query_scope()
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        if self.failing.load(Ordering::Relaxed) {
            return Err(StoreError::from(Disconnected));
        }
        self.payloads.fetch_add(1, Ordering::Relaxed);
        self.inner.clone().get(partition, address).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.queries.fetch_add(addresses.len(), Ordering::Relaxed);
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

/// A client context on `stores` whose remote is `remote`.
pub(crate) fn client_on(stores: &ClientStores, remote: Arc<PagedRemote>) -> Arc<RepositoryContext> {
    client_on_storage(stores, remote, Vec::new())
}

/// A client context on `stores` whose remote is `remote`, with `storage` as the remote's storage.
pub(crate) fn client_on_storage(
    stores: &ClientStores,
    remote: Arc<PagedRemote>,
    storage: Vec<Arc<dyn lore_transport::Storage>>,
) -> Arc<RepositoryContext> {
    Arc::new(client_context(stores, Some(remote), storage))
}

/// A client context on `stores` that holds a write token, with `remote` as its remote or none.
/// Built in the calling execution context, as a command builds its contexts, so a dry run's store
/// discards its writes.
pub(crate) fn writing_client_on(
    stores: &ClientStores,
    remote: Option<Arc<PagedRemote>>,
) -> Arc<RepositoryContext> {
    Arc::new(client_context(stores, remote, Vec::new()).with_write_token(in_memory::token()))
}

fn client_context(
    stores: &ClientStores,
    remote: Option<Arc<PagedRemote>>,
    storage: Vec<Arc<dyn lore_transport::Storage>>,
) -> RepositoryContext {
    let repository_id = lore_revision::lore::RepositoryId::default();
    let remote = match remote {
        Some(remote) => RemoteState::Connected(Connection::detached_revision(
            storage,
            Ok(()),
            repository_id,
            remote,
        )),
        None => RemoteState::Offline,
    };
    RepositoryContext::new_with_state(
        None,
        stores.immutable.clone(),
        stores.mutable.clone(),
        repository_id,
        lore_revision::instance::InstanceId::default(),
        remote,
        Arc::default(),
        None,
    )
}

/// A client context on in-memory stores whose remote is `remote`, and a context over the same
/// stores that can write the revisions the client holds locally.
pub(crate) async fn client_and_writer(
    remote: Arc<PagedRemote>,
) -> (Arc<RepositoryContext>, Arc<RepositoryContext>) {
    let stores = client_stores().await;
    (client_on(&stores, remote), stores.writer)
}

/// A local revision on a branch created from revision 3 of a line only the remote holds.
pub(crate) struct BranchedFromRemoteLine {
    /// The remote's line, revision 1 first.
    pub remote_line: Vec<Hash>,
    pub local: Hash,
    pub client: Arc<RepositoryContext>,
    pub remote: Arc<PagedRemote>,
}

/// The page items for revisions `numbers` of `remote_line`, in the order given.
pub(crate) fn numbered(remote_line: &[Hash], numbers: &[u64]) -> Vec<(u64, Hash)> {
    numbers
        .iter()
        .map(|&number| (number, remote_line[number as usize - 1]))
        .collect()
}

/// [`BranchedFromRemoteLine`] over a remote line of six revisions, which the remote serves
/// from a cached segment holding all six when asked for revision 3.
pub(crate) async fn branched_from_cached_segment() -> BranchedFromRemoteLine {
    branched_from_remote_line(|remote, remote_line| {
        remote.page(
            Anchor::Signature(remote_line[2]),
            numbered(remote_line, &[6, 5, 4, 3, 2, 1]),
            Hash::default(),
        )
    })
    .await
}

/// [`BranchedFromRemoteLine`] over a remote line of six revisions, which the remote walks two
/// at a time from its latest revision. `parent_pages` adds what the remote answers when asked
/// for revision 3, the parent of the local revision.
pub(crate) async fn branched_from_remote_line(
    parent_pages: impl FnOnce(PagedRemote, &[Hash]) -> PagedRemote,
) -> BranchedFromRemoteLine {
    let mut revisions = Vec::new();
    let mut parent = Hash::default();
    for number in 1..=6 {
        let revision = remote_revision(number, parent, Hash::from([0xA0; 32])).await;
        parent = revision.signature;
        revisions.push(revision);
    }
    let remote_line: Vec<Hash> = revisions
        .iter()
        .map(|revision| revision.signature)
        .collect();
    let remote = PagedRemote::new(BranchId::from([7; 16]), remote_line[5])
        .holding(&revisions)
        .page(
            Anchor::Signature(remote_line[5]),
            numbered(&remote_line, &[6, 5]),
            remote_line[3],
        )
        .page(
            Anchor::Signature(remote_line[4]),
            numbered(&remote_line, &[5, 4, 3, 2, 1]),
            Hash::default(),
        )
        .page(
            Anchor::Signature(remote_line[3]),
            numbered(&remote_line, &[4, 3, 2, 1]),
            Hash::default(),
        );
    let remote = Arc::new(parent_pages(remote, &remote_line));
    let (client, writer) = client_and_writer(remote.clone()).await;

    let state = lore_revision::state::State::new();
    state.set_parent_self(remote_line[2]);
    state.set_revision_number(4);
    let token = writer
        .try_write_token()
        .expect("a null context carries a write token");
    let local = state
        .serialize(writer.clone(), token)
        .await
        .expect("serializing the local revision");

    BranchedFromRemoteLine {
        remote_line,
        local,
        client,
        remote,
    }
}

#[async_trait]
impl Revision for PagedRemote {
    async fn branch_create(
        &self,
        _: BranchId,
        _: &str,
        _: &str,
        _: &str,
        _: &[BranchPoint],
    ) -> Result<BranchRecord, ProtocolError> {
        unreachable!("no test of a paged remote creates a branch")
    }

    async fn branch_delete(&self, _: BranchId) -> Result<(), ProtocolError> {
        unreachable!("no test of a paged remote deletes a branch")
    }

    async fn branch_query(
        &self,
        branch: Option<BranchId>,
        name: Option<&str>,
    ) -> Result<BranchQueryResponse, ProtocolError> {
        if let Some(name) = name {
            self.queried_names
                .lock()
                .expect("the queried names' lock")
                .push(name.to_string());
        }
        if let Some(refusal) = &self.refusal {
            return Err(refusal.clone());
        }
        assert!(
            branch.is_none_or(|branch| branch == self.branch),
            "the remote's branch is queried by its id or by a name: got {branch:?}"
        );
        Ok(BranchQueryResponse {
            id: self.branch,
            latest: self.latest,
            metadata: self.metadata,
            ..Default::default()
        })
    }

    async fn branch_push(
        &self,
        _: BranchId,
        _: Hash,
        _: bool,
        _: bool,
    ) -> Result<BranchPushResponse, ProtocolError> {
        unreachable!("no test of a paged remote pushes a branch")
    }

    async fn branch_list(
        &self,
        _: Option<&str>,
        _: bool,
    ) -> Result<BranchListResponse, ProtocolError> {
        unreachable!("no test of a paged remote lists branches")
    }

    async fn revision_list(
        &self,
        start: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        let anchor = match start {
            RevisionListStart::Identifier(identifier) => {
                assert_eq!(
                    identifier.branch, self.branch,
                    "the remote's branch is listed"
                );
                Anchor::Number(identifier.number)
            }
            RevisionListStart::Signature(signature) => Anchor::Signature(signature),
        };
        if let Some((_, failure)) = self.failures.iter().find(|(failing, _)| *failing == anchor) {
            return Err(failure.clone());
        }
        if self.stalls.contains(&anchor) {
            let _abandoned = Abandoned(&self.abandoned);
            self.stalled.notify_one();
            std::future::pending::<()>().await;
        }
        let Some(page) = self.pages.iter().find(|page| page.anchor == anchor) else {
            return Err(ProtocolError::from(NotFound));
        };
        Ok(RevisionListResponse {
            items: page
                .items
                .iter()
                .map(|&(number, signature)| RevisionItem {
                    number,
                    signature,
                    metadata: self
                        .listed_metadata
                        .iter()
                        .find(|(revision, _)| *revision == signature)
                        .map(|&(_, metadata)| metadata)
                        .unwrap_or_default(),
                    state: self
                        .held
                        .iter()
                        .find(|revision| revision.signature == signature)
                        .map(|revision| revision.state.clone())
                        .unwrap_or_default(),
                })
                .collect(),
            next_revision: page.older,
            previous_revision: Hash::default(),
        })
    }

    async fn branch_metadata_get(&self, _: BranchId) -> Result<Hash, ProtocolError> {
        unreachable!("no test of a paged remote reads branch metadata")
    }

    async fn branch_metadata_set(
        &self,
        _: BranchId,
        _: Hash,
        _: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        unreachable!("no test of a paged remote writes branch metadata")
    }
}
