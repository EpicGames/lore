// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A fixture of an in-process Lore Server over in-memory stores, which holds one repository, and a
//! storage handle. Also the helpers that connect to the server, commit through the handle, read at
//! the remote through the handle, collect the events of a call, and run work in a client execution
//! or on the server's stores.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::add::add;
use lore::revision_tree::close::LoreRevisionTreeCloseArgs;
use lore::revision_tree::close::close;
use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
use lore::revision_tree::commit::commit;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
use lore::revision_tree::metadata_set::metadata_set;
use lore::storage::get::LoreStorageGetArgs;
use lore::storage::get::LoreStorageGetItem;
use lore::storage::get::get;
use lore::storage::handle::LoreStore;
use lore_base::error::AddressNotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::metadata::BRANCH;
use lore_revision::node::ROOT_NODE;
use lore_revision::protocol::RemoteConnections;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreMatch;
use lore_storage::immutable_store::query_one;
use lore_transport::Connection;
use tokio::sync::oneshot;

use crate::common::net_common::CountingStore;
use crate::common::net_common::Partitions;
use crate::common::net_common::close_handle;
use crate::common::net_common::make_backends;
use crate::common::net_common::open_handle;
use crate::common::net_common::put_content;
use crate::common::net_common::start_grpc_server;

/// The name of the fixture repository's default branch.
pub(crate) const DEFAULT_BRANCH_NAME: &str = "main";

/// The name of the fixture repository.
const REPOSITORY_NAME: &str = "revision-graph";

/// The creator the fixture records for the repository and its default branch.
const CREATOR: &str = "fixture";

/// An in-process Lore Server over in-memory stores, the repository it holds, and an in-memory
/// storage handle. The server's immutable store isolates partitions, as Lore Server's does.
/// A test ends with [`Fixture::close`]. Dropping the fixture signals the server to stop, and fails
/// the test where the storage handle is still open.
pub(crate) struct Fixture {
    /// The server's URL.
    pub url: String,
    /// The repository the server holds.
    pub repository: Partition,
    /// The repository's default branch, named [`DEFAULT_BRANCH_NAME`], which the fixture creates on
    /// the server alone and with no revision.
    pub default_branch: BranchId,
    /// The storage handle, with the server as its remote unless [`fixture_without_remote`] opened
    /// it.
    pub store: LoreStore,
    /// The immutable store the server serves, which counts its `get`s in
    /// [`server_gets`](Self::server_gets).
    pub server_immutable: Arc<dyn ImmutableStore>,
    /// The mutable store the server serves.
    pub server_mutable: Arc<dyn MutableStore>,
    /// The `get`s made on [`server_immutable`](Self::server_immutable): those the server makes as
    /// it serves calls, and those a test makes on that store. Reads through [`on_server`] are not
    /// counted.
    pub server_gets: Arc<AtomicUsize>,
    /// The store [`server_immutable`](Self::server_immutable) wraps, which [`on_server`] reads
    /// through.
    uncounted_immutable: Arc<dyn ImmutableStore>,
    _shutdown: oneshot::Sender<()>,
}

impl Fixture {
    /// Closes the storage handle, then drops the fixture, which signals the server to stop.
    pub(crate) async fn close(self) {
        close_handle(self.store).await;
    }
}

impl Drop for Fixture {
    /// Panics where the storage handle is still open, so a test that leaves out [`Fixture::close`]
    /// fails. Skips the check where the thread is already panicking, since a second panic would
    /// abort the test binary.
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert!(
                lore::storage::handle::lookup(self.store).is_none(),
                "a test on the fixture ends with `Fixture::close`"
            );
        }
    }
}

/// A fresh [`Fixture`].
pub(crate) async fn fixture() -> Fixture {
    set_up(true).await
}

/// A [`Fixture`] whose storage handle has no remote. The server and its repository are set up as
/// [`fixture`] sets them up, and nothing the handle does reaches the server.
pub(crate) async fn fixture_without_remote() -> Fixture {
    set_up(false).await
}

async fn set_up(with_remote: bool) -> Fixture {
    let (uncounted_immutable, server_mutable) = make_backends(Partitions::Isolated).await;
    let counting = CountingStore::new(uncounted_immutable.clone());
    let server_gets = counting.gets();
    let server_immutable: Arc<dyn ImmutableStore> = counting;

    let (url, shutdown, _stopped) =
        start_grpc_server(server_immutable.clone(), server_mutable.clone());
    let store = open_handle(with_remote.then_some(url.as_str())).await;

    let default_branch = BranchId::from(uuid::Uuid::now_v7());
    let fixture = Fixture {
        url,
        repository: Partition::from(uuid::Uuid::now_v7()),
        default_branch,
        store,
        server_immutable,
        server_mutable,
        server_gets,
        uncounted_immutable,
        _shutdown: shutdown,
    };
    on_server(&fixture, async |context| {
        create_repository(context, default_branch).await;
    })
    .await;
    fixture
}

/// Creates the fixture repository on `context` in the steps Lore Server's repository create takes,
/// with `default_branch` as its default branch. The server's repository registry is left out,
/// since a repository query by id doesn't read it.
async fn create_repository(context: Arc<RepositoryContext>, default_branch: BranchId) {
    let created = lore_revision::util::time::timestamp();
    let metadata = repository::metadata_store(
        context.clone(),
        RepositoryMetadata {
            name: REPOSITORY_NAME.to_string(),
            default_branch,
            default_branch_name: DEFAULT_BRANCH_NAME.to_string(),
            creator: CREATOR.to_string(),
            created,
            ..Default::default()
        },
    )
    .await
    .expect("stores the repository metadata");
    branch::create(
        context.clone(),
        &lore_server::grpc::get_write_token(),
        default_branch,
        DEFAULT_BRANCH_NAME,
        branch::default_category(),
        CREATOR,
        created,
        Vec::new(),
        false,
        false,
    )
    .await
    .expect("creates the default branch");
    repository::metadata_store_hash(context.clone(), metadata)
        .await
        .expect("points the repository at its metadata");
    let id = context.id;
    repository::store_name_to_id(context, REPOSITORY_NAME, id)
        .await
        .expect("maps the repository name to its id");
}

/// Runs `work` on a server context over the server's stores, in a server execution, as Lore Server
/// runs a core. A core that branches on the execution's mode, as `branch::store_latest` does,
/// takes its server path. The context's reads of the immutable store are not counted in
/// [`Fixture::server_gets`].
pub(crate) async fn on_server<T>(
    f: &Fixture,
    work: impl AsyncFnOnce(Arc<RepositoryContext>) -> T,
) -> T {
    let context = Arc::new(RepositoryContext::new_server_context(
        f.uncounted_immutable.clone(),
        f.server_mutable.clone(),
        f.repository,
    ));
    LORE_CONTEXT
        .scope(crate::setup_execution(String::new()), work(context))
        .await
}

/// Runs `work` in a client execution with default global arguments and no event dispatch.
pub(crate) async fn in_client<T>(work: impl Future<Output = T>) -> T {
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::no_dispatch(),
    ));
    LORE_CONTEXT.scope(execution, work).await
}

/// Connects to the server at `url` for `partition`, as a storage handle with that remote and no
/// identity connects.
pub(crate) async fn connection_for(url: &str, partition: Partition) -> Arc<Connection> {
    let connections = RemoteConnections::new(url, None);
    in_client(connections.connection(partition))
        .await
        .expect("connects to the server")
}

/// A list, and a callback that keeps a copy of every event it receives in it. A kept event's
/// `LoreBytes` point at memory that is valid only during the callback, so a test reads none of
/// them.
pub(crate) fn make_sink() -> (Arc<Mutex<Vec<LoreEvent>>>, LoreEventCallback) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let kept = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        kept.lock().unwrap().push(event.clone());
    }));
    (sink, callback)
}

/// The status of a call, from its `Complete` event.
pub(crate) fn status_of(events: &[LoreEvent]) -> i32 {
    events
        .iter()
        .find_map(|event| match event {
            LoreEvent::Complete(data) => Some(data.status),
            _ => None,
        })
        .expect("the call completes")
}

/// The events of a call `run` makes with the callback it is passed. Panics unless the call
/// succeeds.
async fn events_of<F: Future<Output = i32>>(
    what: &str,
    run: impl FnOnce(LoreEventCallback) -> F,
) -> Vec<LoreEvent> {
    let (sink, callback) = make_sink();
    let status = run(callback).await;
    let events = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!((status, status_of(&events)), (0, 0), "{what} must succeed");
    events
}

/// The events of a read of `address` at the remote through `f`'s storage handle, under
/// `correlation_id`, or a fresh one when it is empty. Panics unless the call succeeds.
pub(crate) async fn remote_get(
    f: &Fixture,
    address: Address,
    correlation_id: &str,
) -> Vec<LoreEvent> {
    events_of("reading at the remote", |callback| {
        get(
            LoreGlobalArgs {
                remote: 1,
                correlation_id: LoreString::from(correlation_id),
                ..Default::default()
            },
            LoreStorageGetArgs {
                handle: f.store,
                items: LoreArray::from_vec(vec![LoreStorageGetItem {
                    id: 1,
                    partition: f.repository,
                    address,
                    ..Default::default()
                }]),
            },
            callback,
        )
    })
    .await
}

/// Commits a revision on `branch` through the revision tree API on `store`, and returns its hash.
/// The revision holds the tree of `parent`, zero for a branch's first revision, and one new file
/// under the root, whose content no other file shares. The content is stored in `store` before the
/// commit, and the content and the revision are written to the handle's remote, if it has one.
///
/// The commit moves the branch's latest in the handle's local store alone.
pub(crate) async fn commit_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
) -> Hash {
    commit_file_on(store, repository, branch, parent).await.0
}

/// Commits as [`commit_on`] does. Returns the revision's hash and the address of the file it adds.
async fn commit_file_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
) -> (Hash, Address) {
    let file_id = uuid::Uuid::now_v7();
    let content = file_id.as_bytes();
    let address = put_content(store, repository, Context::from(file_id), content, 1, 0).await;
    let loaded = events_of("loading the parent", |callback| {
        load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store,
                repository,
                revision_hash: parent,
            },
            callback,
        )
    })
    .await;
    let handle = loaded
        .iter()
        .find_map(|event| match event {
            LoreEvent::RevisionTreeLoaded(data) => Some(LoreRevisionTree {
                handle_id: data.handle_id,
            }),
            _ => None,
        })
        .expect("the load reports its handle");

    events_of("adding a file", |callback| {
        add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(vec![new_file(
                    file_id,
                    address,
                    content.len() as u64,
                )]),
            },
            callback,
        )
    })
    .await;
    events_of("naming the branch", |callback| {
        metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                    entry_id: 1,
                    key: LoreString::from(BRANCH),
                    value: LoreMetadata::Context(branch),
                }]),
            },
            callback,
        )
    })
    .await;
    let committed = events_of("committing", |callback| {
        commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: 1,
                handle,
                options: LoreRevisionTreeCommitOptions { remote_write: 1 },
            },
            callback,
        )
    })
    .await;
    let revision = committed
        .iter()
        .find_map(|event| match event {
            LoreEvent::RevisionTreeCommitComplete(data) => Some(data.revision_hash),
            _ => None,
        })
        .expect("the commit reports its revision");

    events_of("closing the tree", |callback| {
        close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs { id: 1, handle },
            callback,
        )
    })
    .await;
    (revision, address)
}

/// A file of `size` bytes under the root, with the content `address` names. It is named for
/// `file_id`, since adding a name the parent already holds fails.
fn new_file(file_id: uuid::Uuid, address: Address, size: u64) -> LoreRevisionTreeAddEntry {
    LoreRevisionTreeAddEntry {
        entry_id: 1,
        parent_node_id: ROOT_NODE,
        parent_entry_index: 0,
        name: LoreString::from(file_id.simple().to_string().as_str()),
        kind: LoreNodeType::File as u32,
        mode: 0o644,
        size,
        address,
    }
}

#[tokio::test]
async fn fixture_serves_branch_query_by_name() {
    let f = fixture().await;

    let branch = in_client(async {
        connection_for(&f.url, f.repository)
            .await
            .revision(f.repository)
            .await
            .expect("reaches the revision service")
            .branch_query(None, Some(DEFAULT_BRANCH_NAME))
            .await
    })
    .await
    .expect("the server holds the default branch");

    assert_eq!(branch.id, f.default_branch);
    assert_eq!(branch.latest, Hash::default());
    f.close().await;
}

#[tokio::test]
async fn fixture_serves_repository_query_by_id() {
    let f = fixture().await;

    let found = in_client(async {
        connection_for(&f.url, f.repository)
            .await
            .repository()
            .await
            .expect("reaches the repository service")
            .query(Some(f.repository), None)
            .await
    })
    .await
    .expect("the server holds the repository");
    let metadata = on_server(&f, async |context| {
        repository::metadata(context, found.metadata).await
    })
    .await
    .expect("the answer names the repository metadata");

    assert_eq!(found.id, f.repository);
    assert_eq!(metadata.default_branch, f.default_branch);
    f.close().await;
}

/// A read at the remote of a revision [`commit_on`] uploaded reaches the server's store through
/// the store [`Fixture::server_gets`] counts.
#[tokio::test]
async fn server_gets_counts_a_remote_read_of_a_committed_revision() {
    let f = fixture().await;
    let revision = commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;
    let before = f.server_gets.load(Ordering::SeqCst);

    let events = remote_get(&f, Address::zero_context_hash(revision), "").await;

    assert!(events.iter().any(|event| matches!(
        event,
        LoreEvent::StorageGetItemComplete(data) if data.error.error_code == 0
    )));
    assert_eq!(f.server_gets.load(Ordering::SeqCst), before + 1);
    f.close().await;
}

#[tokio::test]
async fn fixture_without_remote_keeps_commits_off_the_server() {
    let f = fixture_without_remote().await;
    let handle = lore::storage::handle::lookup(f.store).expect("the handle is open");
    assert!(handle.remote.is_none());

    let revision = commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;
    let held = in_client(query_one(
        &f.server_immutable,
        f.repository,
        Address::zero_context_hash(revision),
    ))
    .await
    .expect("the server's store answers");

    assert_eq!(held.match_made, StoreMatch::MatchNone);
    f.close().await;
}

#[tokio::test]
async fn commit_on_stores_the_file_on_the_server() {
    let f = fixture().await;
    let (_, file) = commit_file_on(f.store, f.repository, f.default_branch, Hash::default()).await;

    let held = on_server(&f, async |context| {
        context.immutable_store().get(context.id, file).await
    })
    .await
    .expect("the server holds the file's content");

    assert_eq!(held.match_made, StoreMatch::MatchFull);
    f.close().await;
}

#[tokio::test]
async fn on_server_reads_leave_server_gets_unchanged() {
    let f = fixture().await;
    let revision = commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;
    let before = f.server_gets.load(Ordering::SeqCst);

    on_server(&f, async |context| {
        context
            .immutable_store()
            .get(context.id, Address::zero_context_hash(revision))
            .await
    })
    .await
    .expect("the server holds the revision");

    assert_eq!(f.server_gets.load(Ordering::SeqCst), before);
    f.close().await;
}

/// A remote read of a committed revision under another partition finds nothing: the server serves
/// content only under the partition it holds it in, as Lore Server does.
#[tokio::test]
async fn fixture_server_isolates_partitions() {
    let f = fixture().await;
    let revision = commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;
    let (sink, callback) = make_sink();

    get(
        LoreGlobalArgs {
            remote: 1,
            ..Default::default()
        },
        LoreStorageGetArgs {
            handle: f.store,
            items: LoreArray::from_vec(vec![LoreStorageGetItem {
                id: 1,
                partition: Partition::from(uuid::Uuid::now_v7()),
                address: Address::zero_context_hash(revision),
                ..Default::default()
            }]),
        },
        callback,
    )
    .await;

    let error_codes: Vec<i32> = sink
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            LoreEvent::StorageGetItemComplete(data) => Some(data.error.error_code),
            _ => None,
        })
        .collect();
    assert_eq!(error_codes, [AddressNotFound::FFI_CODE]);
    f.close().await;
}

/// Dropping a fixture whose storage handle is open panics.
#[tokio::test]
async fn dropping_a_fixture_with_its_handle_open_panics() {
    let f = fixture().await;
    let store = f.store;

    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(f)));

    close_handle(store).await;
    assert!(dropped.is_err());
}

/// Dropping a fixture as its test panics doesn't panic again, so the test's own panic reaches the
/// caller.
#[tokio::test]
async fn a_fixture_dropped_as_its_test_fails_keeps_the_test_panic() {
    let f = fixture().await;
    let store = f.store;

    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _f = f;
        panic!("the test fails");
    }));

    close_handle(store).await;
    assert!(failed.is_err());
}
