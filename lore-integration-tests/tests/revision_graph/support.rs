// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A fixture of an in-process Lore Server over in-memory stores, which holds one repository, and a
//! storage handle.
//!
//! The helpers connect to the server, commit through the handle, read at the remote through the
//! handle, collect the events of a call, and run work in a client execution or on the server's
//! stores. They create branches, read their latests and metadata, write metadata blobs and archive
//! branches at each location, publish revisions on the server, register instances, and register
//! storage handles over the fixture's local stores. They have a mutable store take another
//! writer's metadata once it answers a load, the server's included. They give a revision graph
//! call its global arguments, its arguments and its revision identifier, make the calls of the
//! read verbs, the branch metadata verbs and the branch sync, and pick a call's records, its
//! location complete events and their order, and its synced events, out of its events.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::branch_archive::LoreRevisionGraphBranchArchiveArgs;
use lore::revision_graph::branch_archive::branch_archive;
use lore::revision_graph::branch_create::LoreRevisionGraphBranchCreateArgs;
use lore::revision_graph::branch_create::branch_create;
use lore::revision_graph::branch_list::LoreRevisionGraphBranchListArgs;
use lore::revision_graph::branch_list::branch_list;
use lore::revision_graph::branch_metadata_get::LoreRevisionGraphBranchMetadataGetArgs;
use lore::revision_graph::branch_metadata_get::branch_metadata_get;
use lore::revision_graph::branch_metadata_set::LoreRevisionGraphBranchMetadataEntry;
use lore::revision_graph::branch_metadata_set::LoreRevisionGraphBranchMetadataSetArgs;
use lore::revision_graph::branch_metadata_set::branch_metadata_set;
use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushArgs;
use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushOptions;
use lore::revision_graph::branch_push::branch_push;
use lore::revision_graph::branch_resolve::LoreRevisionGraphBranchResolveArgs;
use lore::revision_graph::branch_resolve::branch_resolve;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncArgs;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
use lore::revision_graph::branch_sync::branch_sync;
use lore::revision_graph::locations::full_context;
use lore::revision_graph::locations::local_write_context;
use lore::revision_graph::locations::remote_read_context;
use lore::revision_graph::revision_resolve::LoreRevisionGraphRevisionResolveArgs;
use lore::revision_graph::revision_resolve::revision_resolve;
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
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::mutable_load::LoreStorageMutableLoadArgs;
use lore::storage::mutable_load::LoreStorageMutableLoadItem;
use lore::storage::mutable_load::mutable_load;
use lore::storage::store::StoreInternal;
use lore_base::error::AddressNotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::test_util::closed_port_url;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::RepositoryId;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::branch::push::MountPolicy;
use lore_revision::branch::push::PushError;
use lore_revision::branch::push::PushReport;
use lore_revision::branch::push::PushRun;
use lore_revision::branch::push::push_branch;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchMetadataEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphInstanceEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphLocationCompleteEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphRevisionEventData;
use lore_revision::instance::InstanceId;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::link::LinkFlags;
use lore_revision::metadata::BRANCH;
use lore_revision::metadata::Metadata;
use lore_revision::node::ROOT_NODE;
use lore_revision::protocol::RemoteConnections;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_revision::repository::SALT_LORE;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
use lore_revision::test_util::register_live_instance;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreMatch;
use lore_storage::immutable_store::query_one;
use lore_storage::local::store_lock::StoreHold;
use lore_storage::test_util::Answered;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::StoreCall;
use lore_storage::test_util::answered;
use lore_transport::BranchRecord;
use lore_transport::Connection;
use tonic::Status;

use crate::common::net_common::CountingStore;
use crate::common::net_common::Partitions;
use crate::common::net_common::TestServerStop;
use crate::common::net_common::close_handle;
use crate::common::net_common::make_backends;
use crate::common::net_common::open_handle;
use crate::common::net_common::put_content;
use crate::common::net_common::start_grpc_server_with;

/// The name of the fixture repository's default branch.
pub(crate) const DEFAULT_BRANCH_NAME: &str = "main";

/// The name of the fixture repository.
const REPOSITORY_NAME: &str = "revision-graph";

/// The creator the fixture records for the repository and its default branch.
pub(crate) const CREATOR: &str = "fixture";

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
    /// The mutable store the server serves, which takes another writer's value once
    /// [`overtake_on_server`] arms it.
    pub server_mutable: Arc<dyn MutableStore>,
    /// The `get`s made on [`server_immutable`](Self::server_immutable): those the server makes as
    /// it serves calls, and those a test makes on that store. Reads through [`on_server`] are not
    /// counted.
    pub server_gets: Arc<AtomicUsize>,
    /// The store [`server_immutable`](Self::server_immutable) wraps, which [`on_server`] reads
    /// through.
    uncounted_immutable: Arc<dyn ImmutableStore>,
    /// What [`server_mutable`](Self::server_mutable) stores once it answers a load, as
    /// [`overtake_on_server`] arms it.
    server_overtake: Overtake,
    _stop: TestServerStop,
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

/// A repository authorizer that refuses every action but [`Action::Read`] with `PermissionDenied`
/// while its flag is set, so a test can seed the repository before it arms it.
pub(crate) struct ReadOnly(pub Arc<AtomicBool>);

#[async_trait]
impl RepositoryAuthorizer for ReadOnly {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        if action != Action::Read && self.0.load(Ordering::SeqCst) {
            return Err(Status::permission_denied(
                "the test's repository is read-only",
            ));
        }
        Ok(())
    }
}

/// A fresh [`Fixture`].
pub(crate) async fn fixture() -> Fixture {
    set_up(true, Arc::new(AllowAllRepositoryAuthorizer)).await
}

/// A [`Fixture`] whose server decides each request's access with `authorizer`.
pub(crate) async fn fixture_with_authorizer(authorizer: Arc<dyn RepositoryAuthorizer>) -> Fixture {
    set_up(true, authorizer).await
}

/// A [`Fixture`] whose storage handle has no remote. The server and its repository are set up as
/// [`fixture`] sets them up, and nothing the handle does reaches the server.
pub(crate) async fn fixture_without_remote() -> Fixture {
    set_up(false, Arc::new(AllowAllRepositoryAuthorizer)).await
}

async fn set_up(with_remote: bool, authorizer: Arc<dyn RepositoryAuthorizer>) -> Fixture {
    let (uncounted_immutable, backend_mutable) = make_backends(Partitions::Isolated).await;
    let counting = CountingStore::new(uncounted_immutable.clone());
    let server_gets = counting.gets();
    let server_immutable: Arc<dyn ImmutableStore> = counting;
    let server_overtake = Overtake::default();
    let server_mutable: Arc<dyn MutableStore> = HookedStore::wrap(
        backend_mutable,
        |_, _, _| Ok(()),
        overtaking_load(server_overtake.clone()),
    );

    let (url, stop) =
        start_grpc_server_with(server_immutable.clone(), server_mutable.clone(), authorizer);
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
        server_overtake,
        _stop: stop,
    };
    on_server(&fixture, async |context| {
        create_repository(context, default_branch).await;
    })
    .await;
    fixture
}

/// A mutable store key and the value another writer stores in it once the store answers a load of
/// the key, as [`overtaking_load`] reads it. `None` stores nothing.
pub(crate) type Overtake = Arc<Mutex<Option<(Hash, Hash)>>>;

/// An `answered` hook for `HookedStore::wrap` that, once the store answers a load of the key
/// `overtake` holds, stores the value it holds in that key, as another writer would after that
/// load, and clears `overtake`.
pub(crate) fn overtaking_load(overtake: Overtake) -> Option<Answered> {
    answered(move |inner, partition, call, key, key_type| {
        let write = if call == StoreCall::Load {
            overtake.lock().unwrap().take_if(|(armed, _)| *armed == key)
        } else {
            None
        };
        async move {
            if let Some((_, value)) = write {
                inner.store(partition, key, value, key_type).await?;
            }
            Ok(())
        }
    })
}

/// Arms the server's mutable store to store `value` in the metadata key of `branch` once it
/// answers the next load of that key, as another writer would after that load.
pub(crate) fn overtake_on_server(f: &Fixture, branch: BranchId, value: Hash) {
    *f.server_overtake.lock().unwrap() = Some((metadata_key(f, branch), value));
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
    create_on_server(
        context.clone(),
        default_branch,
        DEFAULT_BRANCH_NAME,
        branch::default_category(),
        CREATOR,
        created,
        Vec::new(),
    )
    .await;
    repository::metadata_store_hash(context.clone(), metadata)
        .await
        .expect("points the repository at its metadata");
    let id = context.id;
    repository::store_name_to_id(context, REPOSITORY_NAME, id)
        .await
        .expect("maps the repository name to its id");
}

/// Creates `branch`, named `name` in `category` by `creator` at `created`, with `stack`, on the
/// server context `context`, as Lore Server's branch create does.
async fn create_on_server(
    context: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    category: &str,
    creator: &str,
    created: u64,
    stack: Vec<BranchPoint>,
) {
    branch::create(
        context,
        &lore_server::grpc::get_write_token(),
        branch,
        name,
        category,
        creator,
        created,
        stack,
        false,
        false,
    )
    .await
    .expect("creates the branch on the server");
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
        allow_all_repositories(),
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

/// The global arguments of the suite's revision graph calls: no location flag, and the correlation
/// id `revision-graph-tests`.
pub(crate) fn graph_globals() -> LoreGlobalArgs {
    graph_globals_under("revision-graph-tests")
}

/// [`graph_globals`] under `correlation_id`, or under a fresh one where it is empty.
pub(crate) fn graph_globals_under(correlation_id: &str) -> LoreGlobalArgs {
    LoreGlobalArgs {
        correlation_id: LoreString::from(correlation_id),
        ..Default::default()
    }
}

/// [`graph_globals`] with the flag that has a call consult `location` alone: `local` for the local
/// store, `remote` for the remote.
pub(crate) fn graph_globals_at(location: LoreBranchLocation) -> LoreGlobalArgs {
    match location {
        LoreBranchLocation::Local => LoreGlobalArgs {
            local: 1,
            ..graph_globals()
        },
        LoreBranchLocation::Remote => LoreGlobalArgs {
            remote: 1,
            ..graph_globals()
        },
    }
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

/// The status and the events of a call `run` makes with the callback it is passed. Panics where
/// the call sends no `Complete` event, or returns a status other than the one that event carries.
pub(crate) async fn status_and_events_of<F: Future<Output = i32>>(
    run: impl FnOnce(LoreEventCallback) -> F,
) -> (i32, Vec<LoreEvent>) {
    let (sink, callback) = make_sink();
    let status = run(callback).await;
    let events = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(
        status,
        status_of(&events),
        "the call returns the status its `Complete` event carries"
    );
    (status, events)
}

/// The events of a call `run` makes with the callback it is passed. Panics unless the call
/// succeeds, as [`status_and_events_of`] reports it.
pub(crate) async fn events_of<F: Future<Output = i32>>(
    what: &str,
    run: impl FnOnce(LoreEventCallback) -> F,
) -> Vec<LoreEvent> {
    let (status, events) = status_and_events_of(run).await;
    assert_eq!(status, 0, "{what} must succeed");
    events
}

/// The identifier of the latest revision of `branch`.
pub(crate) fn latest(branch: BranchId) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        branch,
        ..Default::default()
    }
}

/// The identifier of revision `number` of `branch`.
pub(crate) fn numbered(branch: BranchId, number: u64) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        branch,
        revision_number: number,
        ..Default::default()
    }
}

/// The identifier of the revision with `hash`.
pub(crate) fn by_hash(hash: Hash) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        hash,
        ..Default::default()
    }
}

/// `value` as a String metadata value.
pub(crate) fn string(value: &str) -> LoreMetadata {
    LoreMetadata::String(LoreString::from(value))
}

/// The arguments of a `revision_resolve` call of `identifier` in `f`'s repository, on `store`.
pub(crate) fn revision_resolve_args(
    f: &Fixture,
    store: LoreStore,
    identifier: LoreRevisionIdentifier,
) -> LoreRevisionGraphRevisionResolveArgs {
    LoreRevisionGraphRevisionResolveArgs {
        id: 1,
        store,
        repository: f.repository,
        revision: identifier,
    }
}

/// The arguments of a `branch_resolve` call of `name` in `f`'s repository, on `store`.
pub(crate) fn branch_resolve_args(
    f: &Fixture,
    store: LoreStore,
    name: &str,
) -> LoreRevisionGraphBranchResolveArgs {
    LoreRevisionGraphBranchResolveArgs {
        id: 1,
        store,
        repository: f.repository,
        name: LoreString::from(name),
    }
}

/// The arguments of a `branch_list` call in `f`'s repository, on `store`: of archived branches too
/// where `include_archived` is set, and only of the branches `creator` created where it isn't
/// empty.
pub(crate) fn branch_list_args(
    f: &Fixture,
    store: LoreStore,
    include_archived: bool,
    creator: &str,
) -> LoreRevisionGraphBranchListArgs {
    LoreRevisionGraphBranchListArgs {
        id: 1,
        store,
        repository: f.repository,
        include_archived: u8::from(include_archived),
        creator: LoreString::from(creator),
    }
}

/// The status and events of a `revision_resolve` call of `identifier` in `f`'s repository, on
/// `store` under `globals`, as [`status_and_events_of`] reports them.
pub(crate) async fn resolve_revision(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    identifier: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    status_and_events_of(|callback| {
        revision_resolve(
            globals,
            revision_resolve_args(f, store, identifier),
            callback,
        )
    })
    .await
}

/// The status and events of a `branch_resolve` call of `name` in `f`'s repository, on `store`
/// under `globals`, as [`status_and_events_of`] reports them.
pub(crate) async fn resolve_branch(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    name: &str,
) -> (i32, Vec<LoreEvent>) {
    status_and_events_of(|callback| {
        branch_resolve(globals, branch_resolve_args(f, store, name), callback)
    })
    .await
}

/// The status and events of a `branch_list` call in `f`'s repository, on `store` under `globals`,
/// as [`status_and_events_of`] reports them: of archived branches too where `include_archived` is
/// set, and only of the branches `creator` created where it isn't empty.
pub(crate) async fn list_branches(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    include_archived: bool,
    creator: &str,
) -> (i32, Vec<LoreEvent>) {
    status_and_events_of(|callback| {
        branch_list(
            globals,
            branch_list_args(f, store, include_archived, creator),
            callback,
        )
    })
    .await
}

/// The status and events of a `branch_archive` call of `branch` in `f`'s repository, on `store`
/// under `globals`, as [`status_and_events_of`] reports them.
pub(crate) async fn archive_branch(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    branch: BranchId,
) -> (i32, Vec<LoreEvent>) {
    let args = LoreRevisionGraphBranchArchiveArgs {
        id: 1,
        store,
        repository: f.repository,
        branch,
    };
    status_and_events_of(|callback| branch_archive(globals, args, callback)).await
}

/// The status and events of a `branch_create` call in `f`'s repository, on `store` under `globals`,
/// of a branch named `name` in the default category from the revision `from` names, as
/// [`status_and_events_of`] reports them.
pub(crate) async fn create_branch(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    name: &str,
    from: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    let args = LoreRevisionGraphBranchCreateArgs {
        id: 1,
        store,
        repository: f.repository,
        name: LoreString::from(name),
        category: LoreString::default(),
        from,
    };
    status_and_events_of(|callback| branch_create(globals, args, callback)).await
}

/// The status and events of a `branch_sync` call of `branch` in `f`'s repository to the revision
/// `target` names, with `options`, on `store` under `globals`, as [`status_and_events_of`] reports
/// them.
pub(crate) async fn sync_branch(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    branch: BranchId,
    target: LoreRevisionIdentifier,
    options: LoreRevisionGraphBranchSyncOptions,
) -> (i32, Vec<LoreEvent>) {
    let args = LoreRevisionGraphBranchSyncArgs {
        id: 1,
        store,
        repository: f.repository,
        branch,
        target,
        options,
    };
    status_and_events_of(|callback| branch_sync(globals, args, callback)).await
}

/// The status and events of a `branch_push` call of `branch` in `f`'s repository, with `options`,
/// on `store` under `globals`, as [`status_and_events_of`] reports them.
pub(crate) async fn push_branch_call(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    branch: BranchId,
    options: LoreRevisionGraphBranchPushOptions,
) -> (i32, Vec<LoreEvent>) {
    let args = LoreRevisionGraphBranchPushArgs {
        id: 1,
        store,
        repository: f.repository,
        branch,
        options,
    };
    status_and_events_of(|callback| branch_push(globals, args, callback)).await
}

/// Pushes the local latest `latest` of `branch` of `f`'s repository with the push core, as a
/// revision graph push runs it, on a context over `f`'s storage handle connected to the server:
/// the branch alone, with its linked revisions checked on the remote, and the local latest
/// written over `latest`. Returns the push's outcome and report.
pub(crate) async fn push_branch_alone(
    f: &Fixture,
    branch: BranchId,
    latest: Hash,
) -> (Result<(), PushError>, PushReport) {
    let store = handle_state(f);
    let remote = store.remote.clone().expect("the handle has a remote");
    in_client(async {
        let context = full_context(&store, f.repository, Some(&remote))
            .await
            .expect("connects to the server");
        let token = context
            .try_write_token()
            .expect("a write context carries a write token");
        let run = PushRun {
            mounts: MountPolicy::RequireOnRemote,
            expected_local_latest: Some(latest),
            ..PushRun::default()
        };
        let mut report = PushReport::default();
        let pushed = push_branch(context.clone(), token, &run, branch, latest, &mut report).await;
        (pushed, report)
    })
    .await
}

/// The status and events of a `branch_metadata_get` call of the metadata of `branch` in `f`'s
/// repository, on `store` under `globals`, as [`status_and_events_of`] reports them: of the entry
/// `key` names, or of every entry where it is empty.
pub(crate) async fn get_branch_metadata(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    branch: BranchId,
    key: &str,
) -> (i32, Vec<LoreEvent>) {
    let args = LoreRevisionGraphBranchMetadataGetArgs {
        id: 1,
        store,
        repository: f.repository,
        branch,
        key: LoreString::from(key),
    };
    status_and_events_of(|callback| branch_metadata_get(globals, args, callback)).await
}

/// The status and events of a `branch_metadata_set` call that sets `entries`, each a key and its
/// value, and removes the keys of `remove` in the metadata of `branch` in `f`'s repository, on
/// `store` under `globals`, as [`status_and_events_of`] reports them.
pub(crate) async fn set_branch_metadata(
    f: &Fixture,
    store: LoreStore,
    globals: LoreGlobalArgs,
    branch: BranchId,
    entries: Vec<(&str, LoreMetadata)>,
    remove: &[&str],
) -> (i32, Vec<LoreEvent>) {
    let entries = entries
        .into_iter()
        .map(|(key, value)| LoreRevisionGraphBranchMetadataEntry {
            key: LoreString::from(key),
            value,
        })
        .collect();
    let args = LoreRevisionGraphBranchMetadataSetArgs {
        id: 1,
        store,
        repository: f.repository,
        branch,
        entries: LoreArray::from_vec(entries),
        remove: LoreArray::from_vec(remove.iter().map(|&key| LoreString::from(key)).collect()),
    };
    status_and_events_of(|callback| branch_metadata_set(globals, args, callback)).await
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

/// Commits as [`commit_on`] does, with the new file named `name`, and returns the revision's hash.
/// Two such commits from one parent conflict.
pub(crate) async fn commit_named_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
    name: &str,
) -> Hash {
    commit_new_file_on(
        store,
        repository,
        branch,
        parent,
        Some(name),
        Hash::default(),
    )
    .await
}

/// Commits on `parent` as [`commit_on`] does, with `parent_other` as the second parent, and returns
/// the merge revision's hash.
pub(crate) async fn commit_merge_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
    parent_other: Hash,
) -> Hash {
    commit_new_file_on(store, repository, branch, parent, None, parent_other).await
}

/// Commits a tree [`tree_with_new_file`] builds, with `parent_other` as the second parent where it
/// isn't zero, and returns the revision's hash.
async fn commit_new_file_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
    name: Option<&str>,
    parent_other: Hash,
) -> Hash {
    let (handle, _) = tree_with_new_file(store, repository, branch, parent, name).await;
    let (status, committed) = commit_tree_merging(handle, parent_other).await;
    assert_eq!(status, 0, "committing must succeed");
    let revision = committed_revision(&committed).expect("the commit reports its revision");
    close_tree(handle).await;
    revision
}

/// Commits as [`commit_on`] does. Returns the revision's hash and the address of the file it adds.
async fn commit_file_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
) -> (Hash, Address) {
    let (handle, address) = tree_with_new_file(store, repository, branch, parent, None).await;
    let (status, committed) = commit_tree(handle).await;
    assert_eq!(status, 0, "committing must succeed");
    let revision = committed_revision(&committed).expect("the commit reports its revision");
    close_tree(handle).await;
    (revision, address)
}

/// The status of a commit on `branch` through the revision tree API on `store`, of the revision
/// [`commit_on`] commits, which fails where the branch's local latest is no longer `parent` when
/// the commit swaps it, and the revision it committed, zero where it failed.
pub(crate) async fn try_commit_on(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
) -> (i32, Hash) {
    let (handle, _) = tree_with_new_file(store, repository, branch, parent, None).await;
    let (status, committed) = commit_tree(handle).await;
    close_tree(handle).await;
    (status, committed_revision(&committed).unwrap_or_default())
}

/// The revision the commit events `committed` report.
fn committed_revision(committed: &[LoreEvent]) -> Option<Hash> {
    committed.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeCommitComplete(data) => Some(data.revision_hash),
        _ => None,
    })
}

/// A tree loaded on `store` at `parent`, with one new file under the root, whose content is stored
/// in `store` and no other file shares, and the `branch` metadata key naming `branch`, and the
/// address of the file's content. The file is named `name`, or for its content where `name` is
/// `None`, since adding a name the parent already holds fails.
async fn tree_with_new_file(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    parent: Hash,
    name: Option<&str>,
) -> (LoreRevisionTree, Address) {
    let file_id = uuid::Uuid::now_v7();
    let name = name.map_or_else(|| file_id.simple().to_string(), str::to_string);
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
                entries: LoreArray::from_vec(vec![new_file(&name, address, content.len() as u64)]),
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
    (handle, address)
}

/// The status and events of a commit of `handle`, which writes the revision to the remote of the
/// tree's storage handle, if it has one.
async fn commit_tree(handle: LoreRevisionTree) -> (i32, Vec<LoreEvent>) {
    commit_tree_merging(handle, Hash::default()).await
}

/// [`commit_tree`] with `parent_other` as the second parent, zero for none.
async fn commit_tree_merging(
    handle: LoreRevisionTree,
    parent_other: Hash,
) -> (i32, Vec<LoreEvent>) {
    status_and_events_of(|callback| {
        commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: 1,
                handle,
                options: LoreRevisionTreeCommitOptions {
                    remote_write: 1,
                    parent_other,
                },
            },
            callback,
        )
    })
    .await
}

/// Closes `handle`.
async fn close_tree(handle: LoreRevisionTree) {
    events_of("closing the tree", |callback| {
        close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 1,
                handle,
                ..Default::default()
            },
            callback,
        )
    })
    .await;
}

/// A file named `name` of `size` bytes under the root, with the content `address` names.
fn new_file(name: &str, address: Address, size: u64) -> LoreRevisionTreeAddEntry {
    LoreRevisionTreeAddEntry {
        entry_id: 1,
        parent_node_id: ROOT_NODE,
        parent_entry_index: 0,
        name: LoreString::from(name),
        kind: LoreNodeType::File as u32,
        mode: 0o644,
        size,
        address,
    }
}

/// Commits a revision on the default branch with [`commit_on`], publishes it, and returns its hash.
/// The default branch's latest is then that revision in the local store and at the remote.
pub(crate) async fn seed_main(f: &Fixture) -> Hash {
    let revision = commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;
    publish(f, f.default_branch, revision).await;
    revision
}

/// Creates a branch named `name` by `creator` in the local store of `f`'s storage handle alone, at
/// the latest of `from` there and with its latest status divergent, as `branch::create` records it,
/// and returns its id.
pub(crate) async fn create_local_branch(
    f: &Fixture,
    name: &str,
    from: BranchId,
    creator: &str,
) -> BranchId {
    let branch = BranchId::from(uuid::Uuid::now_v7());
    create_local_branch_as(f, branch, name, branch::default_category(), from, creator).await;
    branch
}

/// Creates a branch named `name` in `category` by `creator` on the server, as
/// [`create_remote_branch`] does, and in the local store of `f`'s storage handle under the same id,
/// as [`create_local_branch`] does, and returns its id.
pub(crate) async fn create_branch_at_both_locations(
    f: &Fixture,
    name: &str,
    category: &str,
    from: BranchId,
    creator: &str,
) -> BranchId {
    let branch = BranchId::from(uuid::Uuid::now_v7());
    create_remote_branch_as(f, branch, name, category, from, creator).await;
    create_local_branch_as(f, branch, name, category, from, creator).await;
    branch
}

/// Creates `branch` in `category` as [`create_local_branch`] creates a branch.
pub(crate) async fn create_local_branch_as(
    f: &Fixture,
    branch: BranchId,
    name: &str,
    category: &str,
    from: BranchId,
    creator: &str,
) {
    let branch_point = local_latest(f.store, f.repository, from).await;
    let context = local_writer(f);
    in_client(async {
        let stack = stack_from(context.clone(), from, branch_point).await;
        branch::create_record(
            context,
            branch,
            name,
            category,
            creator,
            lore_revision::util::time::timestamp(),
            stack,
            branch_point,
            BranchLatestStatus::Divergent,
            Context::default(),
        )
        .await
    })
    .await
    .expect("creates the branch in the local store");
}

/// Creates a branch named `name` by `creator` on the server alone, at the server's latest of
/// `from`, and returns its id.
pub(crate) async fn create_remote_branch(
    f: &Fixture,
    name: &str,
    from: BranchId,
    creator: &str,
) -> BranchId {
    let branch = BranchId::from(uuid::Uuid::now_v7());
    create_remote_branch_as(f, branch, name, branch::default_category(), from, creator).await;
    branch
}

/// Creates `branch` in `category` as [`create_remote_branch`] creates a branch.
pub(crate) async fn create_remote_branch_as(
    f: &Fixture,
    branch: BranchId,
    name: &str,
    category: &str,
    from: BranchId,
    creator: &str,
) {
    on_server(f, async |context| {
        let branch_point = branch::load_latest(context.clone(), from)
            .await
            .expect("the server holds the branch");
        let stack = stack_from(context.clone(), from, branch_point).await;
        let created = lore_revision::util::time::timestamp();
        create_on_server(context, branch, name, category, creator, created, stack).await;
    })
    .await;
}

/// The stack of a branch created from `from` at `branch_point`, as `branch::child_stack` gives it
/// from the metadata of `from` that `context`'s store holds: that branch point alone where the
/// store holds no metadata of `from`.
async fn stack_from(
    context: Arc<RepositoryContext>,
    from: BranchId,
    branch_point: Hash,
) -> Vec<BranchPoint> {
    let branch_point = BranchPoint {
        branch: from,
        revision: branch_point,
    };
    match branch::metadata_local(context, from).await {
        Ok(metadata) => branch::child_stack(branch_point, &metadata),
        Err(err) if err.is_branch_not_found() => vec![branch_point],
        Err(err) => panic!("reading the metadata of {from} failed: {err}"),
    }
}

/// Moves the server's latest of `branch` to `revision`, which [`commit_on`] wrote to the remote.
pub(crate) async fn publish(f: &Fixture, branch: BranchId, revision: Hash) {
    on_server(f, async |context| {
        let previous = branch::load_latest(context.clone(), branch)
            .await
            .expect("the server holds the branch");
        branch::store_latest(
            context,
            branch,
            previous,
            revision,
            BranchLatestStatus::Convergent,
        )
        .await
    })
    .await
    .expect("moves the server's latest");
}

/// Commits a revision on `branch` from `parent` with [`commit_on`] through a storage handle of its
/// own, whose remote is the server, closes that handle, and returns the revision's hash. The
/// server holds the revision, and its latest of `branch` stays. The local store of `f`'s handle
/// holds none of it, where committing through that handle would leave the revision and the
/// branch's latest there.
pub(crate) async fn commit_from_another_handle(
    f: &Fixture,
    branch: BranchId,
    parent: Hash,
) -> Hash {
    let other = open_handle(Some(&f.url)).await;
    let revision = commit_on(other, f.repository, branch, parent).await;
    close_handle(other).await;
    revision
}

/// Commits a revision on `branch` of `f`'s repository in the local store of `f`'s storage handle
/// alone, on top of `parent`, whose metadata it takes, with an empty tree and a link to
/// `linked_revision` of `linked_repository`, the link the revision tree API can't make. Moves the
/// branch's local latest from `parent` to it, divergent, and returns its hash.
pub(crate) async fn commit_with_link(
    f: &Fixture,
    branch: BranchId,
    parent: Hash,
    linked_repository: RepositoryId,
    linked_revision: Hash,
) -> Hash {
    let context = local_writer(f);
    in_client(async {
        let parent_state = State::deserialize(context.clone(), parent)
            .await
            .expect("reads the parent");
        let state = State::new();
        state.set_parent_self(parent);
        state.set_revision_number(parent_state.revision_number() + 1);
        state.set_metadata_hash(parent_state.metadata_hash());
        state
            .link_add(
                context.clone(),
                linked_repository,
                BranchId::default(),
                linked_revision,
                ROOT_NODE,
                LinkFlags::NoFlags,
            )
            .await
            .expect("links the revision");
        let token = context
            .try_write_token()
            .expect("a write context carries a write token");
        let revision = state
            .serialize(context.clone(), token)
            .await
            .expect("writes the revision");
        branch::store_latest(
            context.clone(),
            branch,
            parent,
            revision,
            BranchLatestStatus::Divergent,
        )
        .await
        .expect("moves the local latest");
        revision
    })
    .await
}

/// Commits a revision with [`commit_from_another_handle`], publishes it, and returns its hash.
pub(crate) async fn publish_from_another_handle(
    f: &Fixture,
    branch: BranchId,
    parent: Hash,
) -> Hash {
    let revision = commit_from_another_handle(f, branch, parent).await;
    publish(f, branch, revision).await;
    revision
}

/// Archives `branch` on the server with `branch::delete`, the core Lore Server's branch delete
/// runs.
pub(crate) async fn archive_on_server(f: &Fixture, branch: BranchId) {
    on_server(f, async |context| branch::delete(context, branch).await)
        .await
        .expect("archives the branch on the server");
}

/// Archives `branch` in the local store of `f`'s storage handle alone, with `branch::delete` on an
/// offline write context over the handle's stores.
pub(crate) async fn archive_locally(f: &Fixture, branch: BranchId) {
    in_client(branch::delete(local_writer(f), branch))
        .await
        .expect("archives the branch in the local store");
}

/// The latest of `branch` the server answers a branch query with. Panics unless the server holds
/// the branch.
pub(crate) async fn server_latest(f: &Fixture, branch: BranchId) -> Hash {
    server_record(f, branch).await.latest
}

/// Commits a revision on the default branch of `f`'s repository on top of `parent` with
/// [`commit_with_link`], linking a revision of a new repository that the local store of `f`'s
/// handle holds and the server doesn't, and returns its hash.
pub(crate) async fn commit_linking_an_unpublished_revision(f: &Fixture, parent: Hash) -> Hash {
    let linked = Partition::from(uuid::Uuid::now_v7());
    let local_only = handle_over_local_stores(f, None);
    let linked_revision = commit_on(
        local_only,
        linked,
        BranchId::from(uuid::Uuid::now_v7()),
        Hash::default(),
    )
    .await;
    close_handle(local_only).await;
    commit_with_link(f, f.default_branch, parent, linked, linked_revision).await
}

/// Whether the server's immutable store holds the revision `revision` of `repository`.
pub(crate) async fn server_holds_revision(
    f: &Fixture,
    repository: Partition,
    revision: Hash,
) -> bool {
    let held = in_client(query_one(
        &f.server_immutable,
        repository,
        Address::zero_context_hash(revision),
    ))
    .await
    .expect("the server's store answers");
    held.match_made != StoreMatch::MatchNone
}

/// The record of `branch` the server answers a branch query with. Panics unless the server holds
/// the branch.
pub(crate) async fn server_record(f: &Fixture, branch: BranchId) -> BranchRecord {
    in_client(async {
        connection_for(&f.url, f.repository)
            .await
            .revision(f.repository)
            .await
            .expect("reaches the revision service")
            .branch_query(Some(branch), None)
            .await
    })
    .await
    .expect("the server holds the branch")
}

/// The metadata of `branch` the server holds, read on its stores with `branch::local_metadata`.
/// Panics unless the server holds metadata for the branch.
pub(crate) async fn server_branch_metadata(f: &Fixture, branch: BranchId) -> Metadata {
    on_server(f, async |context| {
        branch::local_metadata(context, branch).await
    })
    .await
    .expect("reads the branch metadata on the server")
    .expect("the server holds metadata for the branch")
    .1
}

/// The metadata of `branch` in the local store of `f`'s storage handle, read with
/// `branch::local_metadata`. Panics unless the local store holds metadata for the branch.
pub(crate) async fn local_branch_metadata(f: &Fixture, branch: BranchId) -> Metadata {
    in_client(branch::local_metadata(local_writer(f), branch))
        .await
        .expect("reads the branch metadata in the local store")
        .expect("the local store holds metadata for the branch")
        .1
}

/// Writes, through `context`, the metadata of `branch` that the stores of `context` hold with the
/// String `value` set under `key`, as a new blob. Returns the hash the branch's metadata key holds
/// and the new blob's hash, and leaves the key as it is. Panics unless the stores hold metadata for
/// the branch.
pub(crate) async fn metadata_with_entry(
    context: &Arc<RepositoryContext>,
    branch: BranchId,
    key: &str,
    value: &str,
) -> (Hash, Hash) {
    let (held, mut metadata) = branch::local_metadata(context.clone(), branch)
        .await
        .expect("reads the branch metadata")
        .expect("the stores hold metadata for the branch");
    metadata.set_string(key, value).expect("sets the entry");
    let written = metadata
        .serialize(context.clone())
        .await
        .expect("writes the metadata");
    (held, written)
}

/// The latest of `branch` of `repository` in the local store of the storage handle `store`, read
/// with [`latest_by_key`]: zero where the store holds none.
pub(crate) async fn local_latest(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
) -> Hash {
    latest_by_key(store, LoreGlobalArgs::default(), repository, branch).await
}

/// The `function` value of `branch`, such as [`branch::LATEST_STATUS`], in the local store of `f`'s
/// storage handle, read with `branch::mutable_value`: `None` where the store holds none.
pub(crate) async fn local_value(f: &Fixture, function: &str, branch: BranchId) -> Option<Hash> {
    in_client(branch::mutable_value(local_writer(f), function, branch))
        .await
        .expect("loads the value in the local store")
}

/// Stores `latest`, the local latest of `branch`, as its latest status in the local store of `f`'s
/// handle, which records the local latest divergent.
pub(crate) async fn record_divergent(f: &Fixture, branch: BranchId, latest: Hash) {
    in_client(branch::mutable_try_store(
        local_writer(f),
        branch::LATEST_STATUS,
        branch,
        Hash::default(),
        latest,
    ))
    .await
    .expect("records the latest divergent");
}

/// The latest of `branch` of `repository` that a [`mutable_load`] of its latest key reads, through
/// the storage handle `store` under `globals`: zero where the store answers `AddressNotFound`, as
/// the local store does for a key it doesn't hold. Panics on any other failure.
pub(crate) async fn latest_by_key(
    store: LoreStore,
    globals: LoreGlobalArgs,
    repository: Partition,
    branch: BranchId,
) -> Hash {
    let (_, events) = status_and_events_of(|callback| {
        mutable_load(
            globals,
            latest_key_load(store, repository, branch),
            callback,
        )
    })
    .await;
    let loaded = events
        .iter()
        .find_map(|event| match event {
            LoreEvent::StorageMutableLoadItemComplete(data) => {
                Some((data.value, data.error.error_code))
            }
            _ => None,
        })
        .expect("the load completes its item");
    match loaded {
        (latest, 0) => latest,
        (_, code) if code == AddressNotFound::FFI_CODE => Hash::default(),
        (_, code) => panic!("loading the latest failed with code {code}"),
    }
}

/// The arguments of a [`mutable_load`] call, on the storage handle `store`, of the key that holds
/// the latest of `branch` of `repository`, as Lore's key scheme derives it.
pub(crate) fn latest_key_load(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
) -> LoreStorageMutableLoadArgs {
    let (key, key_type) = branch::mutable_key(SALT_LORE, branch::LATEST, repository, branch);
    LoreStorageMutableLoadArgs {
        handle: store,
        items: LoreArray::from_vec(vec![LoreStorageMutableLoadItem {
            id: 1,
            partition: repository,
            key,
            key_type,
        }]),
    }
}

/// The key that holds the metadata of `branch` of `f`'s repository, as Lore's key scheme derives
/// it.
pub(crate) fn metadata_key(f: &Fixture, branch: BranchId) -> Hash {
    branch::mutable_key(SALT_LORE, branch::METADATA, f.repository, branch).0
}

/// Registers a new instance of the fixture repository at `dir` in the local store of `f`'s storage
/// handle, as `lore_revision::test_util::register_live_instance` does, in a client execution.
/// Returns the instance's id.
pub(crate) async fn register_instance_at(
    f: &Fixture,
    branch: BranchId,
    revision: Hash,
    dir: &TempDir,
) -> InstanceId {
    let store = handle_state(f);
    in_client(register_live_instance(
        store.immutable.clone(),
        store.mutable.clone(),
        f.repository,
        branch,
        revision,
        dir.path(),
    ))
    .await
}

/// A second storage handle over the local stores of `f`'s handle, registered with
/// [`register_handle`], with `remote` or no remote. Closing the handle stops garbage collection on
/// those stores, which `f`'s handle still uses; a test closes it with [`close_handle`] before
/// [`Fixture::close`].
pub(crate) fn handle_over_local_stores(
    f: &Fixture,
    remote: Option<Arc<RemoteEndpoint>>,
) -> LoreStore {
    let store = handle_state(f);
    register_handle(f, store.immutable.clone(), store.mutable.clone(), remote)
}

/// A [`handle_over_local_stores`] whose remote is [`closed_port_remote`].
pub(crate) fn handle_over_closed_port(f: &Fixture) -> LoreStore {
    handle_over_local_stores(f, Some(closed_port_remote(f)))
}

/// A remote at a loopback port that refuses connections, reached as the identity of `f`'s handle.
pub(crate) fn closed_port_remote(f: &Fixture) -> Arc<RemoteEndpoint> {
    let store = handle_state(f);
    let identity = Some(store.identity.as_str()).filter(|identity| !identity.is_empty());
    Arc::new(RemoteEndpoint::new(closed_port_url("grpc"), identity))
}

/// A storage handle over `immutable` and `mutable`, with `remote` or no remote. It keeps the
/// identity, bound flags and trust of `f`'s handle, and holds no claim on the stores, as none
/// covers a store in memory.
pub(crate) fn register_handle(
    f: &Fixture,
    immutable: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    remote: Option<Arc<RemoteEndpoint>>,
) -> LoreStore {
    let store = handle_state(f);
    handle::register(Arc::new(StoreInternal::new(
        store.identity.clone(),
        immutable,
        mutable,
        remote,
        store.bound_flags,
        store.trust_remote,
        StoreHold::default(),
    )))
}

/// The state of `f`'s storage handle, which holds its local stores. Panics once the handle is
/// closed.
pub(crate) fn handle_state(f: &Fixture) -> Arc<StoreInternal> {
    handle::lookup(f.store).expect("the fixture's storage handle is open")
}

/// The context `local_write_context` gives over the local stores of `f`'s storage handle: offline,
/// with the in-memory write token. Panics once the handle is closed.
pub(crate) fn local_writer(f: &Fixture) -> Arc<RepositoryContext> {
    local_write_context(&handle_state(f), f.repository)
}

/// The location a revision graph record or location complete event is for.
fn graph_event_location(event: &LoreEvent) -> Option<LoreBranchLocation> {
    match event {
        LoreEvent::RevisionGraphBranch(data) => Some(data.location),
        LoreEvent::RevisionGraphInstance(data) => Some(data.location),
        LoreEvent::RevisionGraphRevision(data) => Some(data.location),
        LoreEvent::RevisionGraphBranchMetadata(data) => Some(data.location),
        LoreEvent::RevisionGraphLocationComplete(data) => Some(data.location),
        _ => None,
    }
}

/// Defines `$name`, which returns the payload of every `$variant` event among `events`, in order.
macro_rules! payloads {
    ($(#[$doc:meta])* $name:ident, $variant:ident, $data:ty) => {
        $(#[$doc])*
        pub(crate) fn $name(events: &[LoreEvent]) -> Vec<&$data> {
            events
                .iter()
                .filter_map(|event| match event {
                    LoreEvent::$variant(data) => Some(data),
                    _ => None,
                })
                .collect()
        }
    };
}

payloads!(
    /// The branch records among a call's events, in order.
    branch_records,
    RevisionGraphBranch,
    LoreRevisionGraphBranchEventData
);
payloads!(
    /// The instance records among a call's events, in order.
    instance_records,
    RevisionGraphInstance,
    LoreRevisionGraphInstanceEventData
);
payloads!(
    /// The revision records among a call's events, in order.
    revision_records,
    RevisionGraphRevision,
    LoreRevisionGraphRevisionEventData
);
payloads!(
    /// The branch metadata records among a call's events, in order.
    branch_metadata_records,
    RevisionGraphBranchMetadata,
    LoreRevisionGraphBranchMetadataEventData
);
payloads!(
    /// The location complete events among a call's events, in order.
    location_completes,
    RevisionGraphLocationComplete,
    LoreRevisionGraphLocationCompleteEventData
);

/// The location, record count and error code of each location complete event among `events`, in
/// order.
pub(crate) fn location_outcomes(events: &[LoreEvent]) -> Vec<(LoreBranchLocation, u64, i32)> {
    location_completes(events)
        .into_iter()
        .map(|complete| {
            (
                complete.location,
                complete.records,
                complete.error.error_code,
            )
        })
        .collect()
}

/// The location of each record and location complete event among `events`, in order, and whether
/// it is a location complete event.
pub(crate) fn event_order(events: &[LoreEvent]) -> Vec<(LoreBranchLocation, bool)> {
    events
        .iter()
        .filter_map(|event| {
            graph_event_location(event).map(|location| {
                (
                    location,
                    matches!(event, LoreEvent::RevisionGraphLocationComplete(_)),
                )
            })
        })
        .collect()
}

/// The pushed events among `events`, each echoing the call's `id`, 1.
pub(crate) fn pushed(events: &[LoreEvent]) -> Vec<Pushed> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranchPushed(data) => {
                assert_eq!(data.id, 1, "a pushed event echoes the call's id");
                Some(Pushed {
                    revision_hash: data.revision_hash,
                    revision_number: data.revision_number,
                    fast_forward_merged: data.fast_forward_merged != 0,
                    remote_latest: data.remote_latest,
                    local_latest: data.local_latest,
                })
            }
            _ => None,
        })
        .collect()
}

/// What a pushed event reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pushed {
    pub revision_hash: Hash,
    pub revision_number: u64,
    pub fast_forward_merged: bool,
    pub remote_latest: Hash,
    pub local_latest: Hash,
}

/// The local latest, remote latest and published revision a pushed event reports for a push that
/// published `revision`, numbered `revision_number`, without a fast-forward merge.
pub(crate) fn published(revision: Hash, revision_number: u64) -> Pushed {
    Pushed {
        revision_hash: revision,
        revision_number,
        fast_forward_merged: false,
        remote_latest: revision,
        local_latest: revision,
    }
}

/// The synced events among `events` as (previous local latest, local latest), each echoing the
/// call's `id`, 1.
pub(crate) fn synced(events: &[LoreEvent]) -> Vec<(Hash, Hash)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranchSynced(data) => {
                assert_eq!(data.id, 1, "a synced event echoes the call's id");
                Some((data.previous_local_latest, data.local_latest))
            }
            _ => None,
        })
        .collect()
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

/// Each branch helper creates its branch at one location alone, at the latest the default branch
/// has there, and neither moves [`Fixture::server_gets`]; `publish` moves the server's latest to a
/// revision `commit_on` uploaded; and a second handle over the fixture's local stores, whose
/// remote refuses connections, reads the latest the fixture's handle reads.
#[tokio::test]
async fn helpers_create_branches_at_each_location_and_publish_a_revision() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    let gets = f.server_gets.load(Ordering::SeqCst);

    let local = create_local_branch(&f, "local", f.default_branch, CREATOR).await;
    let remote = create_remote_branch(&f, "remote", f.default_branch, CREATOR).await;

    assert_eq!(f.server_gets.load(Ordering::SeqCst), gets);
    assert_eq!(local_latest(f.store, f.repository, local).await, committed);
    assert_eq!(server_latest(&f, remote).await, seeded);
    let local_on_server = on_server(&f, async |context| {
        branch::metadata_hash(context, local).await
    })
    .await;
    assert!(
        local_on_server
            .as_ref()
            .is_err_and(|err| err.is_branch_not_found()),
        "expected BranchNotFound, got {local_on_server:?}"
    );

    publish(&f, f.default_branch, committed).await;
    let over_local_stores = handle_over_closed_port(&f);
    let read_through_second_handle =
        local_latest(over_local_stores, f.repository, f.default_branch).await;
    let connect_error = {
        let state = handle::lookup(over_local_stores).expect("the second handle is open");
        let endpoint = state
            .remote
            .clone()
            .expect("the second handle has a remote");
        in_client(remote_read_context(&state, f.repository, &endpoint))
            .await
            .err()
    };
    close_handle(over_local_stores).await;
    let remote_in_local_store = in_client(branch::metadata_hash(local_writer(&f), remote)).await;

    assert_eq!(
        local_latest(f.store, f.repository, remote).await,
        Hash::default()
    );
    assert!(
        remote_in_local_store
            .as_ref()
            .is_err_and(|err| err.is_branch_not_found()),
        "expected BranchNotFound, got {remote_in_local_store:?}"
    );
    assert_eq!(server_latest(&f, f.default_branch).await, committed);
    assert_eq!(read_through_second_handle, committed);
    assert!(
        connect_error
            .as_ref()
            .is_some_and(RevisionGraphError::is_disconnected),
        "expected Disconnected, got {connect_error:?}"
    );
    f.close().await;
}

/// A branch the helpers create from a branch other than the default one records, at each
/// location, its branch point on that branch, then the stack of that branch.
#[tokio::test]
async fn helpers_record_the_stack_of_a_branch_created_from_another_branch() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local_parent = create_local_branch(&f, "local-parent", f.default_branch, CREATOR).await;
    let local_child = create_local_branch(&f, "local-child", local_parent, CREATOR).await;
    let remote_parent = create_remote_branch(&f, "remote-parent", f.default_branch, CREATOR).await;
    let remote_child = create_remote_branch(&f, "remote-child", remote_parent, CREATOR).await;

    let local_stack = in_client(branch::metadata_local(local_writer(&f), local_child))
        .await
        .map(|metadata| branch::stack(&metadata))
        .expect("the local store holds the child's metadata");
    let remote_stack = on_server(&f, async |context| {
        branch::metadata_local(context, remote_child)
            .await
            .map(|metadata| branch::stack(&metadata))
    })
    .await
    .expect("the server holds the child's metadata");

    let on = |branch| BranchPoint {
        branch,
        revision: seeded,
    };
    assert_eq!(local_stack, [on(local_parent), on(f.default_branch)]);
    assert_eq!(remote_stack, [on(remote_parent), on(f.default_branch)]);
    f.close().await;
}

/// An instance `register_instance_at` registers reads as live, and a branch `archive_on_server`
/// archives is listed at the remote as archived.
#[tokio::test]
async fn helpers_register_a_live_instance_and_archive_a_branch_on_the_server() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local = create_local_branch(&f, "local", f.default_branch, CREATOR).await;
    let dir = TempDir::new("revision-graph-live-instance-");
    let instance = register_instance_at(&f, local, seeded, &dir).await;
    let archived = create_remote_branch(&f, "archived", f.default_branch, CREATOR).await;
    archive_on_server(&f, archived).await;

    let (status, events) = list_branches(&f, f.store, graph_globals(), true, "").await;

    assert_eq!(status, 0);
    let archived_records: Vec<_> = branch_records(&events)
        .into_iter()
        .filter(|record| record.branch == archived)
        .map(|record| (record.location, record.archived))
        .collect();
    let instances: Vec<_> = instance_records(&events)
        .into_iter()
        .map(|record| (record.instance_id, record.branch, record.stale))
        .collect();
    assert_eq!(archived_records, [(LoreBranchLocation::Remote, 1)]);
    assert_eq!(instances, [(instance, local, 0)]);
    f.close().await;
}
