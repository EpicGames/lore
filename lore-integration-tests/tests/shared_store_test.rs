// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for shared-store-backed repositories.
//!
//! A shared store backs several repositories with a single immutable store, so what is under
//! test here is the sharing itself: which repositories end up on the same backend, and what that
//! means for content written through one of them.
//!
//! Everything runs against an explicit `shared_store_path` under a tempdir and never sets
//! `make_default`, so no test reads or writes the developer's global config. That matters in both
//! directions — the rest of the suite creates repositories with `LoreSharedStoreMode::Disabled`
//! precisely so it never lands on a shared store by accident (see `storage_test`'s
//! `create_repo`), and these tests must not be the ones that make the machine's config sticky.

#[cfg(all(test, feature = "integration_tests"))]
mod shared_store_tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::repository;
    use lore::storage::open;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_revision::event::LoreEvent;
    use lore_revision::instance::InstanceId;
    use lore_revision::instance::LoreRepositoryInstanceEventData;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::INSTANCE;
    use lore_revision::repository::LoreSharedStoreMode;

    const REMOTE_URL: &str = "lore://localhost/test-shared-store";

    fn globals() -> LoreGlobalArgs {
        LoreGlobalArgs::default()
    }

    fn tempdir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("lore-shared-store-{tag}-"))
            .tempdir()
            .expect("create tempdir")
    }

    fn capture_sink() -> (Arc<Mutex<Vec<LoreEvent>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        (sink, callback)
    }

    fn opened_handle(events: &[LoreEvent]) -> lore::storage::handle::LoreStore {
        let id = events
            .iter()
            .find_map(|e| match e {
                LoreEvent::StorageOpened(data) => Some(data.handle_id),
                _ => None,
            })
            .expect("StorageOpened");
        lore::storage::handle::LoreStore { handle_id: id }
    }

    /// Create the shared store the repositories will attach to.
    ///
    /// `make_default: 0` keeps this out of the global config: setting it would write the store
    /// into the developer's `config.toml` and silently back their real repositories with a
    /// tempdir that disappears when the test ends.
    async fn create_shared_store(path: &Path) {
        let mut store_globals = globals();
        store_globals.offline = 1;
        let status = lore::shared_store::create(
            store_globals,
            lore::shared_store::LoreSharedStoreCreateArgs {
                remote_url: REMOTE_URL.into(),
                path: LoreString::from(path.display().to_string().as_str()),
                make_default: 0,
            },
            None,
        )
        .await;
        assert_eq!(status, 0, "shared store create failed for {path:?}");
    }

    async fn create_repo(repo_path: &Path, mode: LoreSharedStoreMode, shared_path: &Path) {
        create_repo_with(repo_path, mode, shared_path, "", false).await;
    }

    /// Create a repository at `repo_path` and return its ID. A non-empty `id`
    /// (hex) pins the repository ID; `force` replaces an existing `.lore`, as
    /// `lore repository create --force` does.
    async fn create_repo_with(
        repo_path: &Path,
        mode: LoreSharedStoreMode,
        shared_path: &Path,
        id: &str,
        force: bool,
    ) -> RepositoryId {
        let mut repo_globals = globals();
        repo_globals.repository_path = repo_path.into();
        repo_globals.offline = 1;
        repo_globals.force = force as u8;
        let shared_store_path = match mode {
            LoreSharedStoreMode::Enabled => {
                LoreString::from(shared_path.display().to_string().as_str())
            }
            _ => LoreString::default(),
        };
        let (sink, callback) = capture_sink();
        let status = repository::create(
            repo_globals,
            repository::LoreRepositoryCreateArgs {
                repository_url: REMOTE_URL.into(),
                description: LoreString::default(),
                id: LoreString::from(id),
                use_shared_store: mode,
                shared_store_path,
                vfs: Default::default(),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "repository create failed for {repo_path:?}");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .find_map(|e| match e {
                LoreEvent::RepositoryCreate(data) => Some(data.id),
                _ => None,
            })
            .expect("RepositoryCreate")
    }

    /// The instances registered for the repository at `repo_path`, as
    /// `lore repository instance list` reports them.
    async fn list_instances(repo_path: &Path) -> Vec<LoreRepositoryInstanceEventData> {
        let mut list_globals = globals();
        list_globals.repository_path = repo_path.into();
        list_globals.offline = 1;
        let (sink, callback) = capture_sink();
        let status = repository::instance_list(
            list_globals,
            repository::LoreRepositoryInstanceListArgs {},
            callback,
        )
        .await;
        assert_eq!(status, 0, "instance list failed for {repo_path:?}");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .filter_map(|e| match e {
                LoreEvent::RepositoryInstance(data) => Some(data.clone()),
                _ => None,
            })
            .collect()
    }

    /// Run `lore repository instance prune` and return how many instances it removed.
    async fn prune_instances(repo_path: &Path) -> usize {
        let mut prune_globals = globals();
        prune_globals.repository_path = repo_path.into();
        prune_globals.offline = 1;
        let (sink, callback) = capture_sink();
        let status = repository::instance_prune(
            prune_globals,
            repository::LoreRepositoryInstancePruneArgs {},
            callback,
        )
        .await;
        assert_eq!(status, 0, "instance prune failed for {repo_path:?}");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .filter(|e| matches!(e, LoreEvent::RepositoryInstance(_)))
            .count()
    }

    async fn open_repo(repo_path: &Path) -> lore::storage::handle::LoreStore {
        let (sink, callback) = capture_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                in_memory: 0,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "open failed for {repo_path:?}");
        let events = sink.lock().unwrap().clone();
        opened_handle(&events)
    }

    async fn close(handle: lore::storage::handle::LoreStore) {
        let status = lore::storage::close::close(
            globals(),
            lore::storage::close::LoreStorageCloseArgs { handle },
            None,
        )
        .await;
        assert_eq!(status, 0, "close failed");
    }

    fn backend(handle: lore::storage::handle::LoreStore) -> Arc<dyn lore_storage::ImmutableStore> {
        lore::storage::handle::immutable_for_test(handle).expect("backend for open handle")
    }

    /// Two repositories that opt into the same shared store resolve to one immutable store.
    ///
    /// This is the property the whole mode exists for, and the one that silently reshapes an
    /// unrelated test suite when it is inherited from the global config rather than requested.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repositories_sharing_a_store_resolve_to_one_backend() {
        let shared_dir = tempdir("shared");
        create_shared_store(shared_dir.path()).await;

        let repo_a = tempdir("enabled-a");
        let repo_b = tempdir("enabled-b");
        create_repo(
            repo_a.path(),
            LoreSharedStoreMode::Enabled,
            shared_dir.path(),
        )
        .await;
        create_repo(
            repo_b.path(),
            LoreSharedStoreMode::Enabled,
            shared_dir.path(),
        )
        .await;

        let handle_a = open_repo(repo_a.path()).await;
        let handle_b = open_repo(repo_b.path()).await;

        assert!(
            Arc::ptr_eq(&backend(handle_a), &backend(handle_b)),
            "repositories attached to one shared store must share the immutable store",
        );

        close(handle_a).await;
        close(handle_b).await;
    }

    /// `Disabled` keeps each repository on its own backend even when a shared store exists.
    ///
    /// Regression guard for the case that made the storage suite machine-dependent: with
    /// `Inherit` and `use_shared_store_automatically` set, these two repositories would collapse
    /// onto a single store and every identity or lifetime assertion elsewhere would be measuring
    /// a process-wide object.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_keeps_each_repository_on_its_own_backend() {
        let shared_dir = tempdir("unused");
        create_shared_store(shared_dir.path()).await;

        let repo_a = tempdir("disabled-a");
        let repo_b = tempdir("disabled-b");
        create_repo(
            repo_a.path(),
            LoreSharedStoreMode::Disabled,
            shared_dir.path(),
        )
        .await;
        create_repo(
            repo_b.path(),
            LoreSharedStoreMode::Disabled,
            shared_dir.path(),
        )
        .await;

        let handle_a = open_repo(repo_a.path()).await;
        let handle_b = open_repo(repo_b.path()).await;

        assert!(
            !Arc::ptr_eq(&backend(handle_a), &backend(handle_b)),
            "Disabled must not attach the repository to a shared store",
        );

        close(handle_a).await;
        close(handle_b).await;
    }

    /// `lore repository create --force` wipes `.lore` and mints a new instance ID, while the
    /// shared mutable store keeps the registration of the instance it replaced. The instance
    /// list must show the one checkout the directory holds, not both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recreating_a_repository_on_a_shared_store_keeps_one_instance() {
        let shared_dir = tempdir("recreate-shared");
        create_shared_store(shared_dir.path()).await;

        let repo = tempdir("recreate");
        let id = create_repo_with(
            repo.path(),
            LoreSharedStoreMode::Enabled,
            shared_dir.path(),
            "",
            false,
        )
        .await;
        let before = list_instances(repo.path()).await;
        assert_eq!(before.len(), 1, "a fresh repository has one instance");

        create_repo_with(
            repo.path(),
            LoreSharedStoreMode::Enabled,
            shared_dir.path(),
            &id.to_string(),
            true,
        )
        .await;

        let after = list_instances(repo.path()).await;
        let listed: Vec<(InstanceId, String, u8)> = after
            .iter()
            .map(|data| (data.instance_id, data.path.to_string(), data.stale))
            .collect();
        assert_eq!(
            after.len(),
            1,
            "one root directory must be listed once: {listed:?}"
        );
        assert_ne!(
            after[0].instance_id, before[0].instance_id,
            "re-creating the checkout mints a new instance",
        );
        assert_eq!(
            after[0].stale, 0,
            "the surviving entry is the live checkout"
        );
        let on_disk = InstanceId::read_from_file(repo.path().join(DOT_LORE).join(INSTANCE))
            .expect("read .lore/instance");
        assert_eq!(
            after[0].instance_id, on_disk,
            "the listed instance is the one `.lore/instance` names",
        );

        assert_eq!(
            prune_instances(repo.path()).await,
            0,
            "nothing stale is left for prune to remove",
        );
    }

    /// Content written through one repository is readable through another on the same store.
    ///
    /// The sharing is of the content-addressed store, so an address produced by one repository
    /// resolves through the other without any transfer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_written_through_one_repository_reads_back_through_the_other() {
        let shared_dir = tempdir("crossread");
        create_shared_store(shared_dir.path()).await;

        let repo_a = tempdir("cross-a");
        let repo_b = tempdir("cross-b");
        create_repo(
            repo_a.path(),
            LoreSharedStoreMode::Enabled,
            shared_dir.path(),
        )
        .await;
        create_repo(
            repo_b.path(),
            LoreSharedStoreMode::Enabled,
            shared_dir.path(),
        )
        .await;

        let handle_a = open_repo(repo_a.path()).await;
        let handle_b = open_repo(repo_b.path()).await;

        let payload = b"shared across repositories".to_vec();
        let partition = Partition::from([0xC1u8; 16]);
        let context = Context::from([0xC2u8; 16]);
        let address = put_once(handle_a, partition, context, &payload).await;

        let bytes = get_once(handle_b, partition, address).await;
        assert_eq!(
            bytes, payload,
            "the second repository must read the first's content from the shared store",
        );

        close(handle_a).await;
        close(handle_b).await;
    }

    async fn put_once(
        handle: lore::storage::handle::LoreStore,
        partition: Partition,
        context: Context,
        payload: &[u8],
    ) -> lore_base::types::Address {
        let data = lore_revision::event::LoreBytes {
            ptr: payload.as_ptr().cast(),
            len: payload.len(),
        };
        let item = lore::storage::put::LoreStoragePutItem {
            id: 1,
            partition,
            context,
            data,
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let (sink, callback) = capture_sink();
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "put failed");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .find_map(|e| match e {
                LoreEvent::StoragePutItemComplete(data) => Some(data.address),
                _ => None,
            })
            .expect("PUT_ITEM_COMPLETE")
    }

    /// Read one item back, concatenating `GET_DATA` payloads in arrival order.
    ///
    /// The bytes are copied inside the callback: `LoreBytes` borrows a buffer that is released
    /// once the callback returns, so capturing the event and reading `ptr` afterwards would read
    /// freed memory.
    async fn get_once(
        handle: lore::storage::handle::LoreStore,
        partition: Partition,
        address: lore_base::types::Address,
    ) -> Vec<u8> {
        let item = lore::storage::get::LoreStorageGetItem {
            id: 1,
            partition,
            address,
            streaming: 0,
            local_cache: 0,
            ..Default::default()
        };
        let data: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let data_for_cb = data.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageGetData(d) = event
                && d.bytes.len > 0
            {
                let slice =
                    unsafe { std::slice::from_raw_parts(d.bytes.ptr.cast::<u8>(), d.bytes.len) };
                data_for_cb.lock().unwrap().extend_from_slice(slice);
            }
        }));
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "get failed");
        data.lock().unwrap().clone()
    }
}
