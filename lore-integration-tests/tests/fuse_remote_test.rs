// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Reads through a FUSE mount of content only the remote holds: fetch counts, single-flight,
//! the offline deadline, and interruption; and what a virtual clone's background prefetch
//! fetches.
//!
//! The server runs in process, as in `storage_remote_test`, with its store wrapped to count the
//! fetches of the one remote-only file. The tree is committed straight into the server's store,
//! and the mounted repository starts with empty local stores.

#[cfg(all(test, feature = "integration_tests", target_os = "linux"))]
mod fuse_remote_tests {
    use std::collections::HashMap;
    use std::os::fd::AsRawFd as _;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::test_util::TempDir;
    use lore_fuse::abi::Opcode;
    use lore_fuse::mount::unmount_path;
    use lore_fuse::skip_without_fuse;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::fs::fuse::listing_cache::LISTING_BLOCK_BYTES;
    use lore_revision::fs::fuse::listing_cache::LISTING_CACHE_BYTES;
    use lore_revision::fs::fuse::listing_cache::ListingCache;
    use lore_revision::fs::fuse::mount::FuseMount;
    use lore_revision::fs::fuse::mount::FuseMountError;
    use lore_revision::fs::mount_manager::MountManager;
    use lore_revision::fs::mount_manager::MountManagerError;
    use lore_revision::fs::mount_manager_state::MountManagerState;
    use lore_revision::global::external_dir::external_dir_for_instance;
    use lore_revision::instance::InstanceId;
    use lore_revision::instance::InstanceMetadata;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::RepositoryConfig;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryContextCreationArgs;
    use lore_revision::repository::RepositoryPaths;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::repository::SharedStoreToUseConfig;
    use lore_revision::repository::VfsConfig;
    use lore_revision::repository::VfsType;
    use lore_revision::repository::load_repository_config_from_dot_dir;
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use lore_server::authnz::repository_catalog::BaselineRepositoryCatalog;
    use lore_server::grpc::server::FeatureSettings;
    use lore_server::grpc::server::GrpcServerBuilder;
    use lore_server::grpc::server::GrpcTimeouts;
    use lore_server::hooks::HookDispatcher;
    use lore_server::quic::quinn::QuinnConfigBuilder;
    use lore_server::quic::quinn::QuinnServer;
    use lore_server::quic::tests::TestHandlerFactory;
    use lore_server::settings::BaselineAccess;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;

    use crate::common::net_common::bind_matched_pair;
    use crate::setup_execution;

    /// One fragment (under 256 KiB), and a size no tree block has, so fetches of it are
    /// told apart by size alone.
    const FILE_LEN: usize = 250_000;
    const REMOTE_FILE: &str = "remote.bin";
    const LOCAL_FILE: &str = "small.txt";
    const BASE_TIME: u64 = 1_700_000_000_000;
    const PAGE: usize = 4096;
    /// Page-aligned offsets inside the file, one READ each with readahead off.
    const OFFSETS: [usize; 4] = [0, 65_536, 131_072, 196_608];

    /// `FILE_LEN` bytes no other call answers: the tests run in parallel in one process, whose read
    /// flight joins reads of the same content whichever remote they go to.
    fn remote_contents() -> Vec<u8> {
        static CALLS: AtomicU64 = AtomicU64::new(0);
        deterministic_bytes(
            FILE_LEN,
            0x0123_4567_89ab_cdef + 2 * CALLS.fetch_add(1, Ordering::Relaxed),
        )
    }

    /// The server's store, counting (and optionally delaying) fetches of `FILE_LEN` content,
    /// and recording every address it serves.
    struct CountingStore {
        inner: Arc<dyn ImmutableStore>,
        content_gets: AtomicUsize,
        delay_ms: AtomicU64,
        /// Serves `FILE_LEN` content with one byte flipped while set.
        corrupt: AtomicBool,
        served: Mutex<Vec<(lore_base::types::Address, lore_base::types::Fragment)>>,
        /// While set, every `get` waits for `release`.
        held: AtomicBool,
        /// Hashes whose `get` waits for `release`.
        held_hashes: Mutex<Vec<lore_base::types::Hash>>,
        released: tokio::sync::Notify,
        /// How many `get`s wait for `release` now.
        waiting: AtomicUsize,
        /// The hash of every `get`, in the order the store received them.
        arrived: Mutex<Vec<lore_base::types::Hash>>,
    }

    impl CountingStore {
        fn new(inner: Arc<dyn ImmutableStore>) -> Self {
            Self {
                inner,
                content_gets: AtomicUsize::new(0),
                delay_ms: AtomicU64::new(0),
                corrupt: AtomicBool::new(false),
                served: Mutex::new(Vec::new()),
                held: AtomicBool::new(false),
                held_hashes: Mutex::new(Vec::new()),
                released: tokio::sync::Notify::new(),
                waiting: AtomicUsize::new(0),
                arrived: Mutex::new(Vec::new()),
            }
        }

        /// Takes the record of the gets received, in order, leaving it empty.
        fn take_arrived(&self) -> Vec<lore_base::types::Hash> {
            std::mem::take(&mut *self.arrived.lock().expect("arrival record"))
        }

        fn has_arrived(&self, hash: &lore_base::types::Hash) -> bool {
            self.arrived.lock().expect("arrival record").contains(hash)
        }

        /// Makes every `get` wait until `release`.
        fn hold(&self) {
            self.held.store(true, Ordering::SeqCst);
        }

        /// Makes every `get` of an address hashed as one of `hashes` wait until `release`.
        fn hold_hashes(&self, hashes: Vec<lore_base::types::Hash>) {
            *self.held_hashes.lock().expect("held hashes") = hashes;
        }

        fn release(&self) {
            self.held.store(false, Ordering::SeqCst);
            self.held_hashes.lock().expect("held hashes").clear();
            self.released.notify_waiters();
        }

        async fn wait_while_held(&self, hash: &lore_base::types::Hash) {
            let mut counted = None;
            loop {
                let released = self.released.notified();
                if !self.held.load(Ordering::SeqCst)
                    && !self.held_hashes.lock().expect("held hashes").contains(hash)
                {
                    return;
                }
                if counted.is_none() {
                    counted = Some(Waiting::count(&self.waiting));
                }
                released.await;
            }
        }

        /// How many `get`s wait for `release` now.
        fn waiting(&self) -> usize {
            self.waiting.load(Ordering::SeqCst)
        }

        fn content_gets(&self) -> usize {
            self.content_gets.load(Ordering::SeqCst)
        }

        /// Takes the record of served addresses, leaving it empty.
        fn take_served(&self) -> Vec<(lore_base::types::Address, lore_base::types::Fragment)> {
            std::mem::take(&mut *self.served.lock().expect("served record"))
        }

        fn set_delay(&self, delay: Duration) {
            self.delay_ms
                .store(delay.as_millis() as u64, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ImmutableStore for CountingStore {
        /// Joins the claim of the store this one wraps.
        async fn hold_for_command(
            self: Arc<Self>,
        ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, lore_storage::StoreError>
        {
            self.inner.clone().hold_for_command().await
        }

        fn is_local(&self) -> bool {
            self.inner.is_local()
        }

        fn isolates_partitions(&self) -> bool {
            self.inner.isolates_partitions()
        }

        fn read_scope(&self) -> lore_storage::StoreMatch {
            self.inner.read_scope()
        }

        fn query_scope(&self) -> lore_storage::StoreMatch {
            self.inner.query_scope()
        }

        async fn query(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            addresses: &[lore_base::types::Address],
            results: &mut [lore_storage::StoreMatchResult],
        ) -> Result<(), lore_storage::StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn get(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
        ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
            self.arrived
                .lock()
                .expect("arrival record")
                .push(address.hash);
            self.wait_while_held(&address.hash).await;
            let mut data = self.inner.clone().get(partition, address).await?;
            if data.fragment.size_content == FILE_LEN as u64
                && self.corrupt.load(Ordering::SeqCst)
                && let Some(payload) = data.payload.take()
            {
                let mut flipped = payload.to_vec();
                flipped[FILE_LEN / 2] ^= 0xff;
                data.payload = Some(bytes::Bytes::from(flipped));
            }
            self.served
                .lock()
                .expect("served record")
                .push((address, data.fragment));
            if data.fragment.size_content == FILE_LEN as u64 {
                self.content_gets.fetch_add(1, Ordering::SeqCst);
                let delay = self.delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
            Ok(data)
        }

        async fn get_metadata(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
        ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        async fn put(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
            fragment: lore_base::types::Fragment,
            payload: Option<bytes::Bytes>,
            force: bool,
        ) -> Result<(), lore_storage::StoreError> {
            self.inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await
        }

        async fn obliterate(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
            stats: Arc<lore_storage::StoreObliterateStats>,
        ) -> Result<(), lore_storage::StoreError> {
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
        ) -> Result<usize, lore_storage::StoreError> {
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
        ) -> Result<Option<usize>, lore_storage::StoreError> {
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

        async fn flush(
            self: Arc<Self>,
            options: lore_storage::FlushOptions,
        ) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().flush(options).await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().verify(heal).await
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: lore_base::types::Partition,
            source_address: lore_base::types::Address,
            destination_partition: lore_base::types::Partition,
            destination_context: lore_base::types::Context,
            behavior: lore_storage::immutable_store::CopyBehavior,
        ) -> Result<(), lore_storage::StoreError> {
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

    /// One count in a waiting counter, taken back on drop.
    struct Waiting<'a>(&'a AtomicUsize);

    impl<'a> Waiting<'a> {
        fn count(counter: &'a AtomicUsize) -> Self {
            counter.fetch_add(1, Ordering::SeqCst);
            Self(counter)
        }
    }

    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn memory_stores() -> (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) {
        let immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                implicit_durable_stored: true,
                ..Default::default()
            },
        )
        .await
        .expect("in-memory immutable store");
        let mutable: Arc<dyn MutableStore> = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            immutable.clone(),
        )
        .await
        .expect("in-memory mutable store");
        (immutable, mutable)
    }

    /// A `lore://` server over `store`: storage on QUIC, everything else on gRPC, one port.
    struct Server {
        url: String,
        quic: Option<QuinnServer>,
        grpc_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        grpc_stopped: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
    }

    impl Server {
        /// Stops serving: closes the QUIC endpoints and waits until they are idle, then stops the
        /// gRPC server and waits for its report of a clean stop, sent once the serve task holding
        /// the TCP listener has returned.
        async fn shutdown(&mut self) {
            if let Some(quic) = self.quic.take() {
                quic.close().await;
            }
            drop(self.grpc_shutdown.take());
            if let Some(stopped) = self.grpc_stopped.take() {
                tokio::time::timeout(Duration::from_secs(10), stopped)
                    .await
                    .expect("the gRPC server stops")
                    .expect("the gRPC server reports how it stopped")
                    .expect("the gRPC server stops cleanly");
            }
        }
    }

    async fn start_server(
        immutable: Arc<dyn ImmutableStore>,
        mutable: Arc<dyn MutableStore>,
    ) -> Server {
        start_server_with_process_limit(
            immutable,
            mutable,
            lore_server::quic::tests::TEST_PROCESS_LIMIT,
        )
        .await
    }

    /// [`start_server`], handling `process_limit` requests on each stream at once.
    async fn start_server_with_process_limit(
        immutable: Arc<dyn ImmutableStore>,
        mutable: Arc<dyn MutableStore>,
        process_limit: usize,
    ) -> Server {
        let (listener, udp) = bind_matched_pair();
        let addr = listener.local_addr().expect("tcp local addr");
        let (cert_path, key_path, _ca) =
            lore_server::quic::tests::server_certs().expect("test certs");
        let quic = QuinnServer::start(
            QuinnConfigBuilder::new()
                .socket(udp)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .stream_handler_factory(Box::new(TestHandlerFactory::with_process_limit(
                    immutable.clone(),
                    mutable.clone(),
                    process_limit,
                )))
                .build()
                .expect("quinn config"),
        )
        .expect("quinn server start");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (stopped_tx, mut stopped_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let notification_sender: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let catalog = Arc::new(BaselineRepositoryCatalog::new(
                BaselineAccess::Reachable,
                immutable.clone(),
                mutable.clone(),
            ));
            let outcome = GrpcServerBuilder::new()
                .with_environment(EnvironmentConfig::default())
                .with_feature(FeatureSettings::default())
                .with_immutable_store(immutable.clone(), immutable)
                .with_mutable_store(mutable)
                .with_lock_store(None)
                .with_notification(notification_sender, None)
                .with_hook_dispatcher(Arc::new(HookDispatcher::empty()))
                .with_tls_config(None, None, None)
                .unwrap()
                .with_admin_endpoints(HashMap::new(), vec![])
                .with_http2_config(
                    None,
                    None,
                    GrpcTimeouts {
                        request_handler: Duration::from_secs(30),
                        authorization: Duration::from_secs(30),
                    },
                    Default::default(),
                    Default::default(),
                    None,
                )
                .with_token_verification(
                    lore_server::auth::jwt::TokenVerification::Ignore,
                    Arc::new(AllowAllRepositoryAuthorizer),
                    catalog,
                )
                .unwrap()
                .serve_with_listener(listener, async {
                    shutdown_rx.await.ok();
                })
                .await;
            let _ = stopped_tx.send(outcome.map_err(|error| error.to_string()));
        });
        for _ in 0..100 {
            if let Ok(outcome) = stopped_rx.try_recv() {
                panic!("test server on {addr} stopped while starting: {outcome:?}");
            }
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return Server {
                    url: format!("lore://127.0.0.1:{}", addr.port()),
                    quic: Some(quic),
                    grpc_shutdown: Some(shutdown_tx),
                    grpc_stopped: Some(stopped_rx),
                };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server at {addr} never started listening");
    }

    fn creation_args(
        immutable: Arc<dyn ImmutableStore>,
        mutable: Arc<dyn MutableStore>,
        id: RepositoryId,
        instance_id: InstanceId,
        paths: Option<RepositoryPaths>,
        remote: Result<Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
    ) -> RepositoryContextCreationArgs {
        RepositoryContextCreationArgs {
            paths,
            immutable_store: immutable,
            mutable_store: mutable,
            id,
            instance_id,
            remote,
            filter: Arc::default(),
            filesystem_provider: None,
        }
    }

    /// Commits `files` into the server's stores, as a push would leave them, and answers the
    /// revision.
    async fn commit_to_server(
        immutable: Arc<dyn ImmutableStore>,
        mutable: Arc<dyn MutableStore>,
        id: RepositoryId,
        files: &[(&str, &[u8])],
    ) -> lore_base::types::Hash {
        commit_steps_to_server(immutable, mutable, id, &[files]).await
    }

    /// Commits each of `steps` in turn into the server's stores, each commit adding its files to
    /// the tree the one before left, and answers the last revision.
    async fn commit_steps_to_server(
        immutable: Arc<dyn ImmutableStore>,
        mutable: Arc<dyn MutableStore>,
        id: RepositoryId,
        steps: &[&[(&str, &[u8])]],
    ) -> lore_base::types::Hash {
        let dir = TempDir::new("lore-fuse-remote-commit-");
        let path = dir.path().to_path_buf();
        let branch = lore_base::types::Context::from(uuid::Uuid::now_v7());
        let token = RepositoryWriteToken::acquire(&path).await;
        let created = lore_revision::repository::create_local(
            &path,
            &token,
            id,
            branch,
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            lore_revision::repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("committing instance");
        let repository = Arc::new(
            RepositoryContext::new(creation_args(
                immutable,
                mutable,
                id,
                created.instance_id,
                Some(RepositoryPaths::new(
                    path.clone(),
                    path.join(lore_revision::repository::DOT_LORE),
                )),
                Err(lore_transport::ProtocolError::from(
                    lore_base::error::NoRemote,
                )),
            ))
            .with_write_token(token.share()),
        );
        lore_revision::instance::store_current_anchor_branch(&repository, branch)
            .await
            .expect("anchor branch");
        for files in steps {
            for (name, contents) in *files {
                let file = path.join(name);
                // Fixture writes, outside any repository write.
                #[allow(clippy::disallowed_methods)]
                {
                    if let Some(parent) = file.parent() {
                        std::fs::create_dir_all(parent).expect("fixture directory");
                    }
                    std::fs::write(file, contents).expect("fixture file");
                }
            }
            lore_revision::file::stage::stage(
                repository.clone(),
                &token,
                lore_revision::interface::LoreArray::from_vec(vec![
                    lore_revision::interface::LoreString::from(&path),
                ]),
                lore_revision::stage::StageOptions {
                    scan: true,
                    ..Default::default()
                },
            )
            .await
            .expect("stage");
            lore_revision::commit::commit_boxed(
                repository.clone(),
                &token,
                lore_revision::commit::CommitOptions::new("remote fixture".to_string()),
            )
            .await
            .expect("commit");
        }
        let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
            .await
            .expect("anchor");
        revision
    }

    /// Unmounts on drop, lazily if a plain unmount is refused.
    struct UnmountGuard(PathBuf);

    impl Drop for UnmountGuard {
        fn drop(&mut self) {
            if unmount_path(&self.0, false).is_err() {
                let _ = unmount_path(&self.0, true);
            }
        }
    }

    static MANAGER: tokio::sync::OnceCell<Arc<MountManager>> = tokio::sync::OnceCell::const_new();

    /// The service's mount manager, initialised once for this binary.
    async fn mount_manager() -> Arc<MountManager> {
        MANAGER
            .get_or_init(|| async {
                MountManagerState::initialize()
                    .await
                    .expect("mount manager initialises");
                MountManagerState::mount_manager().expect("manager is initialised")
            })
            .await
            .clone()
    }

    /// How long a test's unmount waits out a hold on its mount from outside the test, as a
    /// desktop service probing a new mount under `/tmp` briefly takes.
    const OUTSIDE_HOLD: Duration = Duration::from_secs(5);
    const UNMOUNT_RETRY: Duration = Duration::from_millis(50);

    /// Runs `unmount`, a plain one, again while it finds the mount busy, for at most
    /// [`OUTSIDE_HOLD`]: answers how many times it found the mount busy first, or its last error,
    /// which names the busy paths. Never lazy, so a hold the test itself leaks still fails it.
    async fn waiting_out_holds<E>(
        unmount: impl AsyncFn() -> Result<(), E>,
        busy: impl Fn(&E) -> bool,
    ) -> Result<usize, E> {
        let deadline = Instant::now() + OUTSIDE_HOLD;
        let mut found_busy = 0;
        loop {
            match unmount().await {
                Ok(()) => return Ok(found_busy),
                Err(error) if busy(&error) && Instant::now() < deadline => {
                    found_busy += 1;
                    tokio::time::sleep(UNMOUNT_RETRY).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The manager's plain unmount of the mount at `root`, waiting out an outside hold.
    async fn unmount_through(
        manager: &MountManager,
        root: &Path,
    ) -> Result<usize, MountManagerError> {
        waiting_out_holds(
            async || manager.fuse().unmount(root, false).await,
            |error| matches!(error, MountManagerError::MountsBusy(_)),
        )
        .await
    }

    /// `mount`'s plain unmount, waiting out an outside hold.
    async fn unmount_mount(mount: &FuseMount) -> Result<usize, FuseMountError> {
        waiting_out_holds(
            async || mount.unmount(false).await,
            |error| matches!(error, FuseMountError::MountsBusy(_)),
        )
        .await
    }

    struct Fixture {
        // Field order is drop order: unmount before the directories go.
        mount: Arc<FuseMount>,
        manager: Arc<MountManager>,
        _guard: UnmountGuard,
        root: PathBuf,
        _dir: TempDir,
        _store_dir: TempDir,
        store: Arc<CountingStore>,
        server: Server,
        contents: Vec<u8>,
    }

    impl Fixture {
        fn reads(&self) -> u64 {
            self.mount.metrics().snapshot(Opcode::Read).count
        }

        /// Unmounts, waiting out an outside hold; answers how many times the mount was busy.
        async fn unmount(self) -> usize {
            unmount_through(&self.manager, &self.root)
                .await
                .expect("unmount")
        }
    }

    /// Serves a tree holding a remote-only file, then mounts it as the service does: a FUSE
    /// instance created over an empty shared store, its anchor set to the served revision,
    /// and remounted the way a restarted service brings it back.
    async fn remote_fixture() -> Fixture {
        let contents = remote_contents();
        let fixture = remote_fixture_of(&[(REMOTE_FILE, &contents), (LOCAL_FILE, b"small")]).await;
        // Resolve both entries (and the tree blocks behind them) before anything is counted.
        let root = fixture.root.clone();
        let sizes = blocking(move || {
            [REMOTE_FILE, LOCAL_FILE].map(|name| {
                std::fs::metadata(root.join(name))
                    .expect("stat through the mount")
                    .len()
            })
        })
        .await;
        assert_eq!(sizes, [FILE_LEN as u64, 5]);
        assert_eq!(fixture.store.content_gets(), 0, "stat fetches no content");
        Fixture {
            contents,
            ..fixture
        }
    }

    /// Serves `files` and mounts them as [`remote_fixture`] does, resolving nothing yet.
    async fn remote_fixture_of(files: &[(&str, &[u8])]) -> Fixture {
        remote_fixture_with_process_limit(files, lore_server::quic::tests::TEST_PROCESS_LIMIT).await
    }

    /// [`remote_fixture_of`], its server handling `process_limit` requests on each stream at once.
    async fn remote_fixture_with_process_limit(
        files: &[(&str, &[u8])],
        process_limit: usize,
    ) -> Fixture {
        let (backend_immutable, backend_mutable) = memory_stores().await;
        let store = Arc::new(CountingStore::new(backend_immutable.clone()));
        let served: Arc<dyn ImmutableStore> = store.clone();
        let server =
            start_server_with_process_limit(served, backend_mutable.clone(), process_limit).await;

        let id = RepositoryId::from(uuid::Uuid::now_v7());
        let revision = commit_to_server(backend_immutable, backend_mutable, id, files).await;

        let manager = mount_manager().await;
        let store_dir = TempDir::new("lore-fuse-remote-store-");
        let dir = TempDir::new("lore-fuse-remote-mount-");
        let root = std::fs::canonicalize(dir.path()).expect("canonical mount point");
        let guard = UnmountGuard(root.clone());
        let config = RepositoryConfig {
            remote_url: Some(server.url.clone()),
            shared_store_to_use: Some(SharedStoreToUseConfig {
                use_shared_store: Some(true),
                shared_store_path: Some(store_dir.path().display().to_string()),
            }),
            vfs: Some(VfsConfig {
                vfs_type: VfsType::Fuse,
                base_time: Some(BASE_TIME),
            }),
            ..Default::default()
        };
        let token = RepositoryWriteToken::acquire(&root).await;
        let repository = lore_revision::repository::create_local(
            &root,
            &token,
            id,
            lore_base::types::Context::from(uuid::Uuid::now_v7()),
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            config,
            false,
        )
        .await
        .expect("create_local mounts the FUSE instance");
        lore_revision::instance::store_current_anchor(&repository, revision)
            .await
            .expect("anchor stores");
        repository
            .flush(lore_storage::FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        drop(token);
        unmount_through(&manager, &root).await.expect("unmounts");

        let dot_lore = external_dir_for_instance(repository.instance_id)
            .expect("external dir resolves")
            .join(DOT_LORE);
        let config = load_repository_config_from_dot_dir(&dot_lore).expect("config loads");
        let instance = InstanceMetadata {
            instance_id: repository.instance_id,
            path: root.display().to_string(),
            created: 0,
        };
        manager
            .fuse()
            .remount_instance(repository, &instance, &dot_lore, &config)
            .await
            .expect("the service remounts the instance at the served revision");
        let mount = manager
            .fuse()
            .mounted(&root)
            .expect("the remounted instance");

        Fixture {
            mount,
            manager,
            _guard: guard,
            root,
            _dir: dir,
            _store_dir: store_dir,
            store,
            server,
            contents: Vec::new(),
        }
    }

    async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        lore_base::lore_spawn_blocking!(work)
            .await
            .expect("syscall task must not panic")
    }

    /// Opens `path` with readahead off, so each page read is its own READ.
    fn open_random(path: &Path) -> std::fs::File {
        let file = std::fs::File::open(path).expect("open through the mount");
        // SAFETY: the fd is open for the call.
        let ret = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM) };
        assert_eq!(ret, 0, "posix_fadvise");
        file
    }

    fn pread_page(file: &std::fs::File, offset: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; PAGE];
        // SAFETY: the fd is open and `buf` holds `PAGE` writable bytes.
        let n = unsafe {
            libc::pread(
                file.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                PAGE,
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn open_direct(path: &Path) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)
            .expect("O_DIRECT open through the mount")
    }

    /// One page read into a page-aligned buffer, as `O_DIRECT` requires.
    fn pread_direct(file: &std::fs::File, offset: usize) -> std::io::Result<usize> {
        let layout = std::alloc::Layout::from_size_align(PAGE, PAGE).expect("page layout");
        // SAFETY: `layout` has a non-zero size.
        let buf = unsafe { std::alloc::alloc(layout) };
        assert!(!buf.is_null(), "page allocation");
        // SAFETY: the fd is open and `buf` holds `PAGE` writable bytes.
        let n = unsafe { libc::pread(file.as_raw_fd(), buf.cast(), PAGE, offset as libc::off_t) };
        let result = if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        };
        // SAFETY: `buf` came from `alloc` with this layout.
        unsafe { std::alloc::dealloc(buf, layout) };
        result
    }

    fn execution(name: &str) -> Arc<lore_revision::interface::ExecutionContext> {
        setup_execution(format!("fuse-remote-{name}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn first_read_of_remote_only_file_makes_one_fetch() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("first-read"), async move {
                let fixture = remote_fixture().await;
                let reads_before = fixture.reads();

                let path = fixture.root.join(REMOTE_FILE);
                let pages = blocking(move || {
                    let file = open_random(&path);
                    OFFSETS.map(|offset| pread_page(&file, offset).expect("page read"))
                })
                .await;
                for (offset, page) in OFFSETS.iter().zip(&pages) {
                    assert_eq!(page.as_slice(), &fixture.contents[*offset..*offset + PAGE]);
                }
                assert!(
                    fixture.reads() - reads_before >= OFFSETS.len() as u64,
                    "each page reached the service"
                );
                assert_eq!(
                    fixture.store.content_gets(),
                    1,
                    "the first READ fetches the fragment; later READs find it local"
                );

                let path = fixture.root.join(REMOTE_FILE);
                let whole = blocking(move || std::fs::read(path).expect("whole read")).await;
                assert_eq!(whole, fixture.contents);
                assert_eq!(fixture.store.content_gets(), 1, "no second fetch");
                fixture.unmount().await;
            })
            .await;
    }

    /// A hold on the mount root from outside the test, as a desktop service probing a new mount
    /// under `/tmp` takes, released once the unmount has found the mount busy, does not fail the
    /// unmount.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fixture_unmount_waits_out_a_brief_outside_hold() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("brief-hold"), async move {
                let fixture = remote_fixture().await;
                let held = std::fs::File::open(&fixture.root).expect("open the mount root");
                let (release, busy_seen) = std::sync::mpsc::channel::<()>();
                let releasing = std::thread::spawn(move || {
                    // Released early only where the unmount ended without finding it busy.
                    let _ = busy_seen.recv();
                    drop(held);
                });
                let busy =
                    |error: &MountManagerError| matches!(error, MountManagerError::MountsBusy(_));
                let found_busy = waiting_out_holds(
                    async || {
                        let answer = fixture.manager.fuse().unmount(&fixture.root, false).await;
                        if answer.as_ref().is_err_and(busy) {
                            let _ = release.send(());
                        }
                        answer
                    },
                    busy,
                )
                .await;
                drop(release);
                releasing.join().expect("the hold is released");
                assert!(
                    found_busy.as_ref().is_ok_and(|&found| found > 0),
                    "the unmount answered {found_busy:?}, not after finding the mount held"
                );
            })
            .await;
    }

    /// A hold on the mount root that is never released fails the unmount, naming the mount.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fixture_unmount_fails_under_a_hold_never_released() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("leaked-hold"), async move {
                let fixture = remote_fixture().await;
                let held = std::fs::File::open(&fixture.root).expect("open the mount root");
                let refused = unmount_through(&fixture.manager, &fixture.root).await;
                let root = fixture.root.display().to_string();
                assert!(
                    matches!(&refused, Err(MountManagerError::MountsBusy(busy)) if busy.paths == root),
                    "the unmount answered {refused:?}, not the mount busy"
                );
                drop(held);
                fixture.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cold_reads_of_one_file_fetch_once() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("concurrent"), async move {
                let fixture = remote_fixture().await;
                // Holds the fetch open long enough for every reader to join it.
                fixture.store.set_delay(Duration::from_millis(500));
                let reads_before = fixture.reads();

                let path = fixture.root.join(REMOTE_FILE);
                let pages = blocking(move || {
                    let barrier = Barrier::new(OFFSETS.len());
                    std::thread::scope(|scope| {
                        let readers: Vec<_> = OFFSETS
                            .iter()
                            .map(|&offset| {
                                let barrier = &barrier;
                                let path = &path;
                                scope.spawn(move || {
                                    let file = open_random(path);
                                    barrier.wait();
                                    pread_page(&file, offset)
                                        .unwrap_or_else(|e| panic!("page read at {offset}: {e}"))
                                })
                            })
                            .collect();
                        readers
                            .into_iter()
                            .map(|reader| reader.join().expect("reader completes"))
                            .collect::<Vec<_>>()
                    })
                })
                .await;
                for (offset, page) in OFFSETS.iter().zip(&pages) {
                    assert_eq!(page.as_slice(), &fixture.contents[*offset..*offset + PAGE]);
                }
                assert!(
                    fixture.reads() - reads_before >= 2,
                    "the kernel sent concurrent READs"
                );
                assert_eq!(
                    fixture.store.content_gets(),
                    1,
                    "concurrent misses join one fetch"
                );
                fixture.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn offline_remote_returns_eio_within_deadline() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("offline"), async move {
                let mut fixture = remote_fixture().await;
                let deadline = Duration::from_secs(2);
                fixture.mount.filesystem.set_read_deadline(deadline);
                fixture.server.shutdown().await;
                let reads_before = fixture.reads();

                let path = fixture.root.join(REMOTE_FILE);
                let (result, elapsed) = blocking(move || {
                    let file = open_random(&path);
                    let started = Instant::now();
                    let result = pread_page(&file, 0);
                    (result, started.elapsed())
                })
                .await;
                let Err(error) = result else {
                    panic!("an offline fetch fails");
                };
                assert_eq!(error.raw_os_error(), Some(libc::EIO), "{error}");
                // Nothing answers, so the first READ fails at its deadline. The kernel retries a
                // failed page read once; the retry of an address whose fetch just missed its
                // deadline fails at once, so one deadline bounds both.
                let reads = fixture.reads() - reads_before;
                assert!(reads >= 1, "the read reached the service");
                assert!(
                    elapsed >= deadline && elapsed < deadline + Duration::from_millis(1500),
                    "failed after {elapsed:?} over {reads} READs, deadline {deadline:?}"
                );
                assert_eq!(fixture.store.content_gets(), 0);
                fixture.unmount().await;
            })
            .await;
    }

    static SIGNAL_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    extern "C" fn ignore_signal(_: libc::c_int) {}

    /// Installs a do-nothing SIGINT handler without `SA_RESTART`, restoring the previous one on
    /// drop, so a signalled thread's blocked read is interrupted rather than the process ended.
    struct SigintHandler(libc::sigaction);

    impl SigintHandler {
        fn install() -> Self {
            // SAFETY: `sigaction` is plain old data, valid when zeroed; both pointers are valid
            // and the handler is async-signal-safe (it does nothing).
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = ignore_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&mut action.sa_mask);
                let mut previous: libc::sigaction = std::mem::zeroed();
                assert_eq!(libc::sigaction(libc::SIGINT, &action, &mut previous), 0);
                Self(previous)
            }
        }
    }

    impl Drop for SigintHandler {
        fn drop(&mut self) {
            // SAFETY: restores the action saved at install.
            unsafe { libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut()) };
        }
    }

    /// A READ on the worker of a file whose fetch another READ leads, held at the server,
    /// answers `WouldFetch` at once: the local form never waits on a remote fetch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_worker_read_of_content_being_fetched_defers_without_waiting() {
        use lore_fuse::filesystem::Filesystem as _;
        use lore_fuse::filesystem::RequestInfo;
        use lore_fuse::reply::ReadFault;

        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("worker-never-follows"), async move {
                let fixture = remote_fixture().await;
                let path = fixture.root.join(REMOTE_FILE);
                let ino = blocking({
                    let path = path.clone();
                    move || {
                        std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).expect("stat"))
                    }
                })
                .await;
                fixture.store.hold();
                let cold =
                    lore_base::lore_spawn_blocking!(move || { pread_page(&open_random(&path), 0) });
                tokio::time::timeout(Duration::from_secs(30), async {
                    while fixture.store.waiting() == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the cold READ's fetch reached the server");

                let request = RequestInfo {
                    unique: 1,
                    nodeid: ino,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                };
                let mut out = vec![0u8; PAGE];
                let local = tokio::time::timeout(
                    Duration::from_secs(2),
                    fixture
                        .mount
                        .filesystem
                        .read(&request, ino, 0, 0, &mut out, false),
                )
                .await
                .expect("the worker's READ waited on the held remote fetch");
                assert!(
                    matches!(local, Err(ReadFault::WouldFetch)),
                    "the worker's READ of content not yet local defers"
                );

                fixture.store.release();
                let page = cold
                    .await
                    .expect("the cold READ task")
                    .expect("the cold READ");
                assert_eq!(page.as_slice(), &fixture.contents[..PAGE]);
                fixture.unmount().await;
            })
            .await;
    }

    /// The inode of the file at `name`, the address of its root, and the partition its fragments
    /// load from.
    async fn file_root(
        fixture: &Fixture,
        name: &str,
    ) -> (u64, lore_base::types::Address, lore_base::types::Partition) {
        let path = fixture.root.join(name);
        let ino = blocking(move || {
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).expect("stat"))
        })
        .await;
        let filesystem = &fixture.mount.filesystem;
        let address = match filesystem.inodes().get(ino).as_deref().map(|e| &e.kind) {
            Some(InodeKind::File { address, .. }) => *address,
            _ => panic!("the remote file is a file inode"),
        };
        (ino, address, filesystem.served().repository.id)
    }

    /// A worker READ, with a 2 s hang guard, of the first page of file `ino`; a fault by name.
    async fn worker_read_first_page(fixture: &Fixture, ino: u64) -> Result<Vec<u8>, String> {
        use lore_fuse::filesystem::Filesystem as _;
        use lore_fuse::reply::ReadFault;

        let request = lore_fuse::filesystem::RequestInfo {
            unique: 1,
            nodeid: ino,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let mut out = vec![0u8; PAGE];
        let read = tokio::time::timeout(
            Duration::from_secs(2),
            fixture
                .mount
                .filesystem
                .read(&request, ino, 0, 0, &mut out, false),
        )
        .await
        .expect("the worker's READ waited on the store in flight")
        .map_err(|fault| match fault {
            ReadFault::WouldFetch => "WouldFetch".to_string(),
            ReadFault::Errno(errno) => format!("{errno:?}"),
        })?;
        out.truncate(read);
        Ok(out)
    }

    /// A worker READ of content held locally serves it at once while this process stores the same
    /// address and its upload is held: the store in flight cannot make local bytes wrong. The file
    /// spans a list, whose root the READ loads through the store.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_worker_read_of_local_content_a_store_holds_in_flight_serves_it() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("worker-local-hit-in-flight"), async move {
                let big = deterministic_bytes(2 << 20, 0x5eed);
                let fixture = remote_fixture_of(&[("big.bin", &big)]).await;
                let (ino, address, partition) = file_root(&fixture, "big.bin").await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                lore_revision::immutable::read(
                    repository.clone(),
                    address,
                    None,
                    lore_storage::ReadOptions::default().with_cache(),
                )
                .await
                .expect("the content is fetched into the local store");
                let store = lore_storage::write::try_acquire_in_flight(
                    repository.immutable_store_ref(),
                    partition,
                    address,
                )
                .expect("no store of the file is in flight yet");

                let page = worker_read_first_page(&fixture, ino)
                    .await
                    .expect("the worker serves local content");
                assert_eq!(page.as_slice(), &big[..PAGE]);
                drop(store);
                fixture.unmount().await;
            })
            .await;
    }

    /// A worker READ of content not held locally, while this process stores the same address and
    /// its upload is held, answers `WouldFetch` at once rather than waiting for the store.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_worker_read_of_content_a_store_holds_in_flight_defers() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("worker-miss-in-flight"), async move {
                let fixture = remote_fixture().await;
                let (ino, address, partition) = file_root(&fixture, REMOTE_FILE).await;
                let store = lore_storage::write::try_acquire_in_flight(
                    fixture
                        .mount
                        .filesystem
                        .served()
                        .repository
                        .immutable_store_ref(),
                    partition,
                    address,
                )
                .expect("no store of the file is in flight yet");

                let read = worker_read_first_page(&fixture, ino).await;
                assert_eq!(read, Err("WouldFetch".to_string()));
                drop(store);
                fixture.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sigint_during_fetch_returns_eintr_and_mount_stays_healthy() {
        skip_without_fuse!();
        let _serial = SIGNAL_SERIAL.lock().await;
        LORE_CONTEXT
            .scope(execution("sigint"), async move {
                let fixture = remote_fixture().await;
                let fetch_delay = Duration::from_secs(5);
                fixture.store.set_delay(fetch_delay);
                let _handler = SigintHandler::install();

                let path = fixture.root.join(REMOTE_FILE);
                let (tid_tx, tid_rx) = std::sync::mpsc::channel();
                // Direct I/O: a buffered read's READ is a background request, which the kernel
                // never interrupts; only fatal signals end the wait for its page.
                let reader = std::thread::spawn(move || {
                    let file = open_direct(&path);
                    // SAFETY: `pthread_self` has no preconditions.
                    tid_tx
                        .send(unsafe { libc::pthread_self() })
                        .expect("send tid");
                    let started = Instant::now();
                    let result = pread_direct(&file, 0);
                    (result, started.elapsed())
                });
                let tid = tid_rx.recv().expect("reader started");

                let waited = Instant::now();
                while fixture.store.content_gets() == 0 {
                    assert!(
                        waited.elapsed() < Duration::from_secs(10),
                        "the fetch never reached the server"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // SAFETY: `tid` names the reader thread, alive until joined below.
                assert_eq!(unsafe { libc::pthread_kill(tid, libc::SIGINT) }, 0);
                let (result, elapsed) =
                    blocking(move || reader.join().expect("reader joins")).await;

                let Err(error) = result else {
                    panic!("an interrupted fetch fails");
                };
                assert_eq!(error.raw_os_error(), Some(libc::EINTR), "{error}");
                assert!(
                    elapsed < fetch_delay,
                    "returned after {elapsed:?}, before the fetch could finish"
                );

                // The mount keeps serving: a listing, a local read, and the same file again.
                fixture.store.set_delay(Duration::ZERO);
                let root = fixture.root.clone();
                let (names, small, remote) = blocking(move || {
                    let mut names: Vec<String> = std::fs::read_dir(&root)
                        .expect("listing")
                        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
                        .collect();
                    names.sort();
                    let small = std::fs::read(root.join(LOCAL_FILE)).expect("local read");
                    let remote = std::fs::read(root.join(REMOTE_FILE)).expect("remote read");
                    (names, small, remote)
                })
                .await;
                assert_eq!(names, vec![REMOTE_FILE, LOCAL_FILE]);
                assert_eq!(small, b"small");
                assert_eq!(remote, fixture.contents);
                fixture.unmount().await;
            })
            .await;
    }

    /// Content over two levels of fragment lists: more leaves than one 256 KiB list holds.
    const DEEP_FILE_LEN: usize = 576 << 20;

    /// `len` bytes of an xorshift seeded with `seed | 1`: seeds differing only in their lowest bit
    /// give the same bytes.
    fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_ne_bytes());
        }
        out.truncate(len);
        out
    }

    /// Asserts the tree under `root` lists exactly `expected`'s files and reads each back as
    /// committed, one file at a time.
    fn assert_tree_reads_back(root: &Path, expected: &[(String, Vec<u8>)]) {
        let mut listed = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("listing through the mount") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    listed.push(
                        path.strip_prefix(root)
                            .expect("under the root")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        listed.sort();
        let names: Vec<&str> = expected.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(listed, names, "the tree lists as committed");
        for (name, contents) in expected {
            let read = std::fs::read(root.join(name)).expect("read through the mount");
            assert!(read == *contents, "{name} reads back as committed");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_fetched_fragment_is_cached_locally_whatever_the_context_says() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("always-cache"), async move {
                let deep = deterministic_bytes(DEEP_FILE_LEN, 0x5eed_0001);
                let shallow = deterministic_bytes(300_000, 0x5eed_0002);
                let expected: Vec<(String, Vec<u8>)> = vec![
                    ("a/b/c/shallow.bin".to_string(), shallow),
                    ("a/b/leaf.txt".to_string(), b"one leaf".to_vec()),
                    ("a/small.txt".to_string(), b"small".to_vec()),
                    ("deep.bin".to_string(), deep),
                ];
                let files: Vec<(&str, &[u8])> = expected
                    .iter()
                    .map(|(name, contents)| (name.as_str(), contents.as_slice()))
                    .collect();
                let fixture = remote_fixture_of(&files).await;

                // A context that leaves fetched content out of the local store; the provider
                // must cache regardless.
                let filesystem = fixture.mount.filesystem.clone();
                let published = filesystem.served();
                let uncached = published.repository.without_command_locks();
                uncached.set_disable_cache(true);
                let tree = published
                    .state
                    .tree(published.repository.clone())
                    .await
                    .expect("tree");
                filesystem.publish(
                    published.state.clone(),
                    filesystem.provider_repository(&uncached),
                    Some(Arc::new(ListingCache::new(
                        LISTING_CACHE_BYTES,
                        LISTING_BLOCK_BYTES,
                    ))),
                    Some(&tree),
                );
                drop(published);
                fixture.store.take_served();

                let expected = Arc::new(expected);
                let (root, tree) = (fixture.root.clone(), expected.clone());
                let deep_ino = blocking(move || {
                    assert_tree_reads_back(&root, &tree);
                    std::os::unix::fs::MetadataExt::ino(
                        &std::fs::metadata(root.join("deep.bin")).expect("stat"),
                    )
                })
                .await;

                let served = fixture.store.take_served();
                let repository = filesystem.served().repository.clone();
                let local = repository.immutable_store();
                let deep = match filesystem
                    .inodes()
                    .get(deep_ino)
                    .as_deref()
                    .map(|e| &e.kind)
                {
                    Some(InodeKind::File { address, .. }) => *address,
                    _ => panic!("deep.bin is a file inode"),
                };
                let (root_list, payload) = lore_storage::load_fragment(
                    local.clone(),
                    repository.id,
                    deep,
                    lore_storage::ReadOptions::default().no_remote(),
                    None,
                )
                .await
                .expect("deep.bin's root list is local");
                let first_child = lore_base::types::Address {
                    hash: lore_base::types::Hash::from(&payload[..32]),
                    context: deep.context,
                };
                let (sub_list, _) = lore_storage::load_fragment(
                    local.clone(),
                    repository.id,
                    first_child,
                    lore_storage::ReadOptions::default().no_remote(),
                    None,
                )
                .await
                .expect("deep.bin's first child is local");
                let is_list = |fragment: lore_base::types::Fragment| {
                    fragment.flags & lore_storage::FragmentFlags::PayloadFragmented != 0
                };
                assert!(
                    is_list(root_list) && is_list(sub_list),
                    "deep.bin is two levels of lists"
                );
                let mut missing = Vec::new();
                for (address, _) in &served {
                    let stored = local
                        .clone()
                        .get(repository.id, *address)
                        .await
                        .is_ok_and(|data| data.payload.is_some());
                    if !stored {
                        missing.push(*address);
                    }
                }
                assert!(
                    missing.is_empty(),
                    "{} of {} served fragments are not in the local store, e.g. {:?}",
                    missing.len(),
                    served.len(),
                    missing.first()
                );

                // A fresh mount has no page cache: the second pass is all local.
                unmount_mount(&fixture.mount).await.expect("unmounts");
                let remounted = fixture.mount.remount().await.expect("remounts");
                let root = fixture.root.clone();
                blocking(move || assert_tree_reads_back(&root, &expected)).await;
                assert_eq!(
                    fixture.store.take_served().len(),
                    0,
                    "the second pass fetches nothing"
                );
                unmount_mount(&remounted).await.expect("unmounts");
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_load_rejects_a_corrupt_remote_payload() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("bad-remote-hash"), async move {
                let fixture = remote_fixture().await;
                let path = fixture.root.join(REMOTE_FILE);
                let ino = blocking(move || {
                    std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).expect("stat"))
                })
                .await;
                let filesystem = fixture.mount.filesystem.clone();
                let address = match filesystem.inodes().get(ino).as_deref().map(|e| &e.kind) {
                    Some(InodeKind::File { address, .. }) => *address,
                    _ => panic!("the remote file is a file inode"),
                };
                let repository = filesystem.served().repository.clone();
                fixture.store.corrupt.store(true, Ordering::SeqCst);

                let result = lore_revision::immutable::read(
                    repository.clone(),
                    address,
                    None,
                    lore_storage::ReadOptions::default().with_cache(),
                )
                .await;
                assert!(result.is_err(), "a payload failing its hash is not served");
                assert!(fixture.store.content_gets() >= 1, "the remote was asked");
                let cached = repository
                    .immutable_store()
                    .get(repository.id, address)
                    .await
                    .is_ok_and(|data| data.payload.is_some());
                assert!(!cached, "the corrupt payload is not stored locally");
                fixture.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remote_payload_that_fails_its_hash_is_rejected_and_not_cached() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("bad-remote-hash"), async move {
                let fixture = remote_fixture().await;
                fixture.store.corrupt.store(true, Ordering::SeqCst);
                let path = fixture.root.join(REMOTE_FILE);
                let result = blocking(move || {
                    let file = open_random(&path);
                    pread_page(&file, 0)
                })
                .await;
                let error = result.expect_err("a payload failing its hash is not served");
                assert_eq!(error.raw_os_error(), Some(libc::EIO), "{error}");

                let repository = fixture.mount.filesystem.served().repository.clone();
                let served = fixture.store.take_served();
                let (address, _) = served
                    .iter()
                    .find(|(_, fragment)| fragment.size_content == FILE_LEN as u64)
                    .expect("the file's fragment was fetched");
                let cached = repository
                    .immutable_store()
                    .get(repository.id, *address)
                    .await
                    .is_ok_and(|data| data.payload.is_some());
                assert!(!cached, "the corrupt payload is not stored locally");
                fixture.unmount().await;
            })
            .await;
    }

    /// The file inode `name` resolves to under the mount's root, as an address.
    async fn file_address(fixture: &Fixture, name: &str) -> lore_base::types::Address {
        let path = fixture.root.join(name);
        let ino = blocking(move || {
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).expect("stat"))
        })
        .await;
        match fixture
            .mount
            .filesystem
            .inodes()
            .get(ino)
            .as_deref()
            .map(|e| &e.kind)
        {
            Some(InodeKind::File { address, .. }) => *address,
            _ => panic!("{name} is a file inode"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_stores_verified_payloads_and_skips_a_corrupt_one() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-corrupt"), async move {
                let good_contents = deterministic_bytes(100_000, 0x0fed_cba9_8765_4321);
                let fixture = remote_fixture_of(&[
                    (REMOTE_FILE, &remote_contents()),
                    ("good.bin", &good_contents),
                ])
                .await;
                let bad = file_address(&fixture, REMOTE_FILE).await;
                let good = file_address(&fixture, "good.bin").await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                fixture.store.corrupt.store(true, Ordering::SeqCst);

                lore_revision::immutable::cache_counted(repository.clone(), vec![bad, good], true)
                    .await
                    .and_then(lore_revision::immutable::Cached::into_result)
                    .expect_err("cache answers the fragment it could not verify");

                let stored = |address| {
                    let repository = repository.clone();
                    async move {
                        repository
                            .immutable_store()
                            .get(repository.id, address)
                            .await
                            .is_ok_and(|data| data.payload.is_some())
                    }
                };
                assert!(fixture.store.content_gets() >= 1, "the remote was asked");
                assert!(!stored(bad).await, "the corrupt payload is not stored");
                assert!(stored(good).await, "the verified payload is stored");
                fixture.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_counts_the_payload_bytes_it_fetches() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-bytes"), async move {
                let contents = deterministic_bytes(100_000, 0x0bad_cafe_f00d_0001);
                let fixture = remote_fixture_of(&[("good.bin", &contents)]).await;
                let good = file_address(&fixture, "good.bin").await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                fixture.store.take_served();

                let cached =
                    lore_revision::immutable::cache_counted(repository.clone(), vec![good], true)
                        .await
                        .expect("cache is best effort past its connection");
                let served: u64 = fixture
                    .store
                    .take_served()
                    .iter()
                    .map(|(_, fragment)| u64::from(fragment.size_payload))
                    .sum();
                assert_eq!(cached.fragments, 1);
                assert!(served > 0, "the remote served the file");
                assert_eq!(cached.bytes, served);
                fixture.unmount().await;
            })
            .await;
    }

    /// A file of many fragments, under the readahead bound.
    const FRAGMENTED_LEN: usize = 8 << 20;
    const FRAGMENTED_FILE: &str = "fragmented.bin";

    /// The subfragments of the fragment list at `address`, read from the server's store.
    async fn subfragments(
        fixture: &Fixture,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
    ) -> Vec<lore_base::types::Address> {
        let data = fixture
            .store
            .inner
            .clone()
            .get(partition, address)
            .await
            .expect("the server holds the list");
        let payload = data.payload.expect("the list's payload");
        let (_, list) = lore_storage::read::verify_receipt(data.fragment, payload, address)
            .expect("the list verifies");
        let list = list.to_vec();
        list.as_chunks::<{ size_of::<lore_base::types::FragmentReference>() }>()
            .0
            .iter()
            .map(|reference| lore_base::types::Address {
                context: address.context,
                hash: lore_base::types::Hash::from(
                    <[u8; 32]>::try_from(&reference[..32]).expect("a hash"),
                ),
            })
            .collect()
    }

    /// Whether the local store holds `address`'s payload.
    async fn held_locally(
        repository: &Arc<RepositoryContext>,
        address: lore_base::types::Address,
    ) -> bool {
        repository
            .immutable_store()
            .get(repository.id, address)
            .await
            .is_ok_and(|data| data.payload.is_some())
    }

    /// A file whose fragment list is local but none of whose fragments are is cached whole: the
    /// local list's subfragments are fetched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_fetches_the_subfragments_of_a_list_already_local() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-local-list"), async move {
                let contents = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0001);
                let fixture = remote_fixture_of(&[(FRAGMENTED_FILE, &contents)]).await;
                let root = file_address(&fixture, FRAGMENTED_FILE).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                let children = subfragments(&fixture, repository.id, root).await;
                assert!(children.len() > 1, "the file is fragmented");
                let list = fixture
                    .store
                    .inner
                    .clone()
                    .get(repository.id, root)
                    .await
                    .expect("the server holds the list");
                repository
                    .immutable_store()
                    .put(repository.id, root, list.fragment, list.payload, false)
                    .await
                    .expect("the list is stored locally");
                for &child in &children {
                    assert!(!held_locally(&repository, child).await);
                }

                let cached =
                    lore_revision::immutable::cache_counted(repository.clone(), vec![root], true)
                        .await
                        .expect("cache is best effort past its connection");
                assert_eq!(
                    cached.fragments,
                    children.len(),
                    "every subfragment is fetched"
                );
                for &child in &children {
                    assert!(held_locally(&repository, child).await, "{child} is local");
                }

                // A walk of the file now local fetches nothing; it costs two local lookups per
                // fragment and one list read.
                fixture.store.take_served();
                let walked = Instant::now();
                let again =
                    lore_revision::immutable::cache_counted(repository.clone(), vec![root], true)
                        .await
                        .expect("cache is best effort past its connection");
                println!(
                    "walk of a local list of {} fragments: {:?}",
                    children.len(),
                    walked.elapsed()
                );
                assert_eq!(again.fragments, 0);
                assert!(fixture.store.take_served().is_empty(), "nothing is fetched");
                fixture.unmount().await;
            })
            .await;
    }

    /// Two sequential READs of a cold fragmented file start a readahead that caches the whole
    /// file, though the first READ already stored its fragment list locally: a READ far into it
    /// then fetches nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn readahead_after_a_cold_read_fetches_the_whole_file() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("readahead-whole"), async move {
                let contents = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0002);
                let fixture = remote_fixture_of(&[(FRAGMENTED_FILE, &contents)]).await;
                let root = file_address(&fixture, FRAGMENTED_FILE).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                let children = subfragments(&fixture, repository.id, root).await;
                assert!(children.len() > 1, "the file is fragmented");

                let path = fixture.root.join(FRAGMENTED_FILE);
                let file = blocking(move || {
                    let file = open_random(&path);
                    pread_page(&file, 0).expect("first page");
                    pread_page(&file, PAGE).expect("second page");
                    file
                })
                .await;
                let filesystem = fixture.mount.filesystem.clone();
                assert_eq!(
                    wait_for_count(|| filesystem.readaheads_started() as usize, 1).await,
                    1,
                    "two sequential READs start one readahead"
                );
                let waited = Instant::now();
                let mut missing = children.len();
                while missing > 0 && waited.elapsed() < Duration::from_secs(30) {
                    missing = 0;
                    for &child in &children {
                        missing += usize::from(!held_locally(&repository, child).await);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert_eq!(missing, 0, "readahead caches every fragment");

                fixture.store.take_served();
                let far = FRAGMENTED_LEN - 2 * PAGE;
                let page = blocking(move || pread_page(&file, far).expect("far page")).await;
                assert_eq!(page, contents[far..far + PAGE]);
                assert!(
                    fixture.store.take_served().is_empty(),
                    "a READ far into the file fetches nothing"
                );
                fixture.unmount().await;
            })
            .await;
    }

    /// A stored object: its address, fragment and payload.
    type Stored = (
        lore_base::types::Address,
        lore_base::types::Fragment,
        bytes::Bytes,
    );

    /// `content` as a plain fragment under `context`.
    fn leaf(context: lore_base::types::Context, content: Vec<u8>) -> Stored {
        let payload = bytes::Bytes::from(content);
        let address = lore_base::types::Address {
            context,
            hash: lore_storage::hash::hash_slice(&payload),
        };
        let fragment = lore_base::types::Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        (address, fragment, payload)
    }

    /// A fragment list of `children` under `context`.
    fn list_of(context: lore_base::types::Context, children: &[Stored]) -> Stored {
        let mut payload = Vec::new();
        let mut offset = 0;
        for (address, fragment, _) in children {
            let reference = lore_base::types::FragmentReference {
                hash: address.hash,
                offset_content: offset,
            };
            payload.extend_from_slice(zerocopy::IntoBytes::as_bytes(&reference));
            offset += fragment.size_content;
        }
        let payload = bytes::Bytes::from(payload);
        let address = lore_base::types::Address {
            context,
            hash: lore_storage::hash::hash_slice(&payload),
        };
        let fragment = lore_base::types::Fragment {
            flags: lore_storage::FragmentFlags::PayloadFragmented.bits(),
            size_payload: payload.len() as u32,
            size_content: offset,
        };
        (address, fragment, payload)
    }

    /// A local root list whose children are lists, one local and one not, each over leaves the
    /// local store lacks but one, is cached whole by one call: every leaf ends up local.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_walks_a_list_of_lists_to_every_leaf() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-list-of-lists"), async move {
                let fixture = remote_fixture_of(&[(LOCAL_FILE, b"small")]).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                let context = lore_base::types::Context::from(uuid::Uuid::now_v7());
                let leaves: Vec<Stored> = (0..8u64)
                    .map(|seed| leaf(context, deterministic_bytes(1000, 0x1eaf_0000 + 2 * seed)))
                    .collect();
                let local_list = list_of(context, &leaves[..4]);
                let remote_list = list_of(context, &leaves[4..]);
                let root = list_of(context, &[local_list.clone(), remote_list.clone()]);

                let server = fixture.store.inner.clone();
                let local = repository.immutable_store();
                let everything = leaves
                    .iter()
                    .chain([&local_list, &remote_list, &root])
                    .map(|stored| (server.clone(), stored));
                let held = [&root, &local_list, &leaves[0]]
                    .into_iter()
                    .map(|stored| (local.clone(), stored));
                for (store, (address, fragment, payload)) in everything.chain(held) {
                    store
                        .put(
                            repository.id,
                            *address,
                            *fragment,
                            Some(payload.clone()),
                            false,
                        )
                        .await
                        .expect("stored");
                }
                for (address, _, _) in &leaves[1..] {
                    assert!(!held_locally(&repository, *address).await);
                }

                lore_revision::immutable::cache_counted(repository.clone(), vec![root.0], true)
                    .await
                    .expect("cache is best effort past its connection");
                for (index, (address, _, _)) in leaves.iter().enumerate() {
                    assert!(
                        held_locally(&repository, *address).await,
                        "leaf {index} is local"
                    );
                }
                fixture.unmount().await;
            })
            .await;
    }

    /// A file whose fragment list the local store indexes without its payload, as size eviction
    /// leaves it, and none of whose fragments are local, is cached whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_fetches_a_list_whose_local_payload_was_evicted() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-evicted-list"), async move {
                let contents = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0004);
                let fixture = remote_fixture_of(&[(FRAGMENTED_FILE, &contents)]).await;
                let root = file_address(&fixture, FRAGMENTED_FILE).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                let children = subfragments(&fixture, repository.id, root).await;
                let list = fixture
                    .store
                    .inner
                    .clone()
                    .get(repository.id, root)
                    .await
                    .expect("the server holds the list");
                repository
                    .immutable_store()
                    .put(repository.id, root, list.fragment, None, false)
                    .await
                    .expect("the list is indexed without its payload");
                let mut matches = [lore_storage::StoreMatchResult::default()];
                repository
                    .immutable_store()
                    .query(repository.id, &[root], &mut matches)
                    .await
                    .expect("the query");
                assert!(
                    matches[0].match_made != lore_storage::StoreMatch::MatchNone
                        && !matches[0].stored_local,
                    "the list is indexed, its payload not local: {:?}",
                    matches[0]
                );
                for &child in &children {
                    assert!(!held_locally(&repository, child).await);
                }

                lore_revision::immutable::cache_counted(repository.clone(), vec![root], true)
                    .await
                    .expect("cache is best effort past its connection");
                assert!(held_locally(&repository, root).await, "the list is local");
                for &child in &children {
                    assert!(held_locally(&repository, child).await, "{child} is local");
                }
                fixture.unmount().await;
            })
            .await;
    }

    /// A cache that leaves subfragments alone, as the tree prefetch's block warm-up asks, still
    /// fetches an entry the local store indexes without its payload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_without_subfragments_fetches_an_entry_whose_payload_was_evicted() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-evicted-plain"), async move {
                let fixture = remote_fixture_of(&[(REMOTE_FILE, &remote_contents())]).await;
                let address = file_address(&fixture, REMOTE_FILE).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                let stored = fixture
                    .store
                    .inner
                    .clone()
                    .get(repository.id, address)
                    .await
                    .expect("the server holds the file");
                repository
                    .immutable_store()
                    .put(repository.id, address, stored.fragment, None, false)
                    .await
                    .expect("the file is indexed without its payload");
                assert!(!held_locally(&repository, address).await);

                let cached = lore_revision::immutable::cache_counted(
                    repository.clone(),
                    vec![address],
                    false,
                )
                .await
                .expect("cache is best effort past its connection");
                assert_eq!(cached.fragments, 1);
                assert!(
                    held_locally(&repository, address).await,
                    "the file is local"
                );
                fixture.unmount().await;
            })
            .await;
    }

    /// A demand READ issued while the remote holds a cache of a large file reaches it behind at
    /// most the cap of cache gets: the cache sends no more than the cap at once, and the
    /// READ's priority get takes no part in it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_demand_read_behind_a_held_prefetch_waits_for_at_most_the_cap() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cache-cap"), async move {
                let cap = TEST_CAP;
                assert!(cap > 0, "the cap is on");
                let large = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0006);
                let contents = remote_contents();
                // Each stream admits more than the cap, so the server never hides a backlog the
                // client sends.
                let fixture = remote_fixture_with_process_limit(
                    &[(FRAGMENTED_FILE, &large), (REMOTE_FILE, &contents)],
                    4 * cap,
                )
                .await;
                let root = file_address(&fixture, FRAGMENTED_FILE).await;
                let demand = file_address(&fixture, REMOTE_FILE).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                cap_remote(&repository, cap).await;
                let children = subfragments(&fixture, repository.id, root).await;
                assert!(
                    children.len() > 2 * cap,
                    "{} fragments against a cap of {cap}",
                    children.len()
                );
                // The list is local, so the cache asks for every fragment at once.
                let list = fixture
                    .store
                    .inner
                    .clone()
                    .get(repository.id, root)
                    .await
                    .expect("the server holds the list");
                repository
                    .immutable_store()
                    .put(repository.id, root, list.fragment, list.payload, false)
                    .await
                    .expect("the list is stored locally");

                fixture.store.hold();
                fixture.store.take_arrived();
                let prefetch = {
                    let repository = repository.clone();
                    lore_base::lore_spawn!(async move {
                        lore_revision::immutable::cache_counted(repository, vec![root], true).await
                    })
                };
                let mut held = wait_for_count(|| fixture.store.waiting(), cap).await;
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let now = fixture.store.waiting();
                    if now == held {
                        break;
                    }
                    held = now;
                }

                let path = fixture.root.join(REMOTE_FILE);
                let read =
                    lore_base::lore_spawn_blocking!(move || { pread_page(&open_random(&path), 0) });
                let waited = Instant::now();
                while !fixture.store.has_arrived(&demand.hash)
                    && waited.elapsed() < Duration::from_secs(10)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let arrived = fixture.store.take_arrived();
                fixture.store.release();
                let page = read.await.expect("the READ task").expect("the demand page");
                assert_eq!(page, contents[..PAGE]);
                prefetch
                    .await
                    .expect("the cache task")
                    .expect("cache is best effort past its connection");

                let ahead = arrived
                    .iter()
                    .position(|hash| *hash == demand.hash)
                    .expect("the demand READ reached the remote");
                assert!(
                    ahead <= cap,
                    "{ahead} cache gets reached the remote before the demand READ; cap {cap}"
                );
                fixture.unmount().await;
            })
            .await;
    }

    /// A fragment that turns local while its cache get waits for a permit, as a demand READ stores
    /// one, is not fetched again: the get finds it local once it has the permit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fragment_stored_while_its_get_waits_for_a_permit_is_not_fetched_again() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cap-recheck"), async move {
                let cap = TEST_CAP;
                let large = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_000a);
                let fixture = remote_fixture_of(&[(FRAGMENTED_FILE, &large)]).await;
                let root = file_address(&fixture, FRAGMENTED_FILE).await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                cap_remote(&repository, cap).await;
                let children = subfragments(&fixture, repository.id, root).await;
                assert!(children.len() > cap);
                store_list_locally(&fixture, &repository, root).await;

                fixture.store.hold();
                fixture.store.take_arrived();
                let cache = {
                    let repository = repository.clone();
                    lore_base::lore_spawn!(async move {
                        lore_revision::immutable::cache_counted(repository, vec![root], true).await
                    })
                };
                assert_eq!(wait_for_count(|| fixture.store.waiting(), cap).await, cap);
                let waiting = *children
                    .iter()
                    .find(|child| !fixture.store.has_arrived(&child.hash))
                    .expect("a fragment whose get waits for a permit");
                let stored = fixture
                    .store
                    .inner
                    .clone()
                    .get(repository.id, waiting)
                    .await
                    .expect("the server holds the fragment");
                repository
                    .immutable_store()
                    .put(
                        repository.id,
                        waiting,
                        stored.fragment,
                        stored.payload,
                        false,
                    )
                    .await
                    .expect("the fragment turns local, as a demand READ stores it");

                fixture.store.release();
                let cached = cache
                    .await
                    .expect("the cache task")
                    .expect("cache is best effort past its connection");
                assert!(
                    !fixture.store.take_arrived().contains(&waiting.hash),
                    "the fragment that turned local is not fetched"
                );
                assert_eq!(cached.already_local, 1);
                fixture.unmount().await;
            })
            .await;
    }

    /// A fragment list that turns local, stored alone as a demand READ stores it, while its cache
    /// get waits for a permit, is still walked: every fragment of its file is cached.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_list_stored_while_its_get_waits_for_a_permit_is_walked() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cap-recheck-list"), async move {
                let cap = TEST_CAP;
                let first = deterministic_bytes(FRAGMENTED_LEN / 8, 0x5eed_0000_0000_000c);
                let second = deterministic_bytes(FRAGMENTED_LEN / 8, 0x5eed_0000_0000_000e);
                let fixture =
                    remote_fixture_of(&[("first.bin", &first), ("second.bin", &second)]).await;
                let roots = [
                    file_address(&fixture, "first.bin").await,
                    file_address(&fixture, "second.bin").await,
                ];
                let repository = fixture.mount.filesystem.served().repository.clone();
                let permits = cap_remote(&repository, cap).await;
                // One permit left: one root's get takes it and is held at the server, so the
                // other waits for a permit, its local query already answered.
                let taken = permits
                    .clone()
                    .acquire_many_owned(u32::try_from(cap - 1).expect("a small cap"))
                    .await
                    .expect("the permits");

                fixture.store.hold();
                fixture.store.take_arrived();
                let cache = {
                    let repository = repository.clone();
                    lore_base::lore_spawn!(async move {
                        lore_revision::immutable::cache_counted(repository, roots.to_vec(), true)
                            .await
                    })
                };
                assert_eq!(wait_for_count(|| fixture.store.waiting(), 1).await, 1);
                let waiting = *roots
                    .iter()
                    .find(|root| !fixture.store.has_arrived(&root.hash))
                    .expect("one root waits for a permit");
                let children = subfragments(&fixture, repository.id, waiting).await;
                assert!(children.len() > 1, "the file is fragmented");
                store_list_locally(&fixture, &repository, waiting).await;

                drop(taken);
                fixture.store.release();
                let cached = cache
                    .await
                    .expect("the cache task")
                    .expect("cache is best effort past its connection");
                assert_eq!(
                    cached.already_local, 1,
                    "the list turned local while it waited"
                );
                for &child in &children {
                    assert!(held_locally(&repository, child).await, "{child} is local");
                }
                fixture.unmount().await;
            })
            .await;
    }

    /// Concurrent cache calls on one remote share its cap: two held at once have no more than the
    /// cap of gets in flight between them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_caches_on_one_remote_share_its_cap() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cap-shared"), async move {
                let cap = TEST_CAP;
                let first = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0010);
                let second = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0012);
                let fixture = remote_fixture_with_process_limit(
                    &[("first.bin", &first), ("second.bin", &second)],
                    4 * cap,
                )
                .await;
                let repository = fixture.mount.filesystem.served().repository.clone();
                cap_remote(&repository, cap).await;
                let mut roots = Vec::new();
                for name in ["first.bin", "second.bin"] {
                    let root = file_address(&fixture, name).await;
                    assert!(subfragments(&fixture, repository.id, root).await.len() > cap);
                    store_list_locally(&fixture, &repository, root).await;
                    roots.push(root);
                }
                fixture.store.hold();
                let mut caches = Vec::new();
                for root in roots {
                    let repository = repository.clone();
                    caches.push(lore_base::lore_spawn!(async move {
                        lore_revision::immutable::cache_counted(repository, vec![root], true).await
                    }));
                }
                let mut held = wait_for_count(|| fixture.store.waiting(), cap).await;
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let now = fixture.store.waiting();
                    if now == held {
                        break;
                    }
                    held = now;
                }
                fixture.store.release();
                for cache in caches {
                    cache
                        .await
                        .expect("the cache task")
                        .expect("cache is best effort past its connection");
                }
                assert_eq!(held, cap, "both caches together hold the cap");
                fixture.unmount().await;
            })
            .await;
    }

    /// The cap the cap tests set on their own remote, so their files stay small whatever the
    /// default.
    const TEST_CAP: usize = 16;

    /// Fixes the cache get cap of `repository`'s remote at `cap` and answers its permits: a
    /// connection takes the cap of the first call for them, and nothing in a fixture caches before
    /// its test does.
    async fn cap_remote(
        repository: &Arc<RepositoryContext>,
        cap: usize,
    ) -> Arc<tokio::sync::Semaphore> {
        let permits = repository
            .remote()
            .await
            .expect("the remote")
            .cache_get_permits(cap)
            .expect("a cap");
        assert_eq!(
            permits.available_permits(),
            cap,
            "the test sets its remote's cap before anything caches"
        );
        permits
    }

    /// Stores the fragment list at `root` locally, so a cache of it asks for every fragment at
    /// once.
    async fn store_list_locally(
        fixture: &Fixture,
        repository: &Arc<RepositoryContext>,
        root: lore_base::types::Address,
    ) {
        let list = fixture
            .store
            .inner
            .clone()
            .get(repository.id, root)
            .await
            .expect("the server holds the list");
        repository
            .immutable_store()
            .put(repository.id, root, list.fragment, list.payload, false)
            .await
            .expect("the list is stored locally");
    }

    /// Cache gets are capped per remote: a cache held at one remote's cap leaves a cache from
    /// another remote free to fetch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remote_held_at_its_cap_leaves_another_remote_fetching() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cap-per-remote"), async move {
                let cap = TEST_CAP;
                let large = deterministic_bytes(FRAGMENTED_LEN, 0x5eed_0000_0000_0008);
                let held = remote_fixture_of(&[(FRAGMENTED_FILE, &large)]).await;
                let free = remote_fixture_of(&[(REMOTE_FILE, &remote_contents())]).await;
                let root = file_address(&held, FRAGMENTED_FILE).await;
                let held_repository = held.mount.filesystem.served().repository.clone();
                cap_remote(&held_repository, cap).await;
                assert!(subfragments(&held, held_repository.id, root).await.len() > cap);
                store_list_locally(&held, &held_repository, root).await;

                held.store.hold();
                let stalled = {
                    let repository = held_repository.clone();
                    lore_base::lore_spawn!(async move {
                        lore_revision::immutable::cache_counted(repository, vec![root], true).await
                    })
                };
                assert_eq!(
                    wait_for_count(|| held.store.waiting(), cap).await,
                    cap,
                    "the held remote takes the cap"
                );

                let address = file_address(&free, REMOTE_FILE).await;
                let free_repository = free.mount.filesystem.served().repository.clone();
                cap_remote(&free_repository, cap).await;
                let cached = tokio::time::timeout(
                    Duration::from_secs(20),
                    lore_revision::immutable::cache_counted(
                        free_repository.clone(),
                        vec![address],
                        true,
                    ),
                )
                .await
                .expect("the other remote's cache ends while the first is held")
                .expect("cache is best effort past its connection");
                assert_eq!(cached.fragments, 1);
                assert!(held_locally(&free_repository, address).await);

                held.store.release();
                stalled
                    .await
                    .expect("the cache task")
                    .expect("cache is best effort past its connection");
                free.unmount().await;
                held.unmount().await;
            })
            .await;
    }

    // ---------------------------------------------------------------------------------------------
    // Write requests that wait on the remote: copy-ups and namespace changes in cold directories
    // ---------------------------------------------------------------------------------------------

    /// More requests held at once than the mount runs workers.
    const HELD: usize = 12;

    /// What each held first write writes at offset 0.
    const EDIT: &[u8] = b"edited";

    /// The size each held truncate leaves.
    const TRUNCATED: u64 = FILE_LEN as u64 / 2;

    /// What each held FALLOCATE allocates past the end of its file.
    const ALLOCATED: usize = 4096;

    /// The kinds of first change [`held_change`] makes.
    const HELD_KINDS: usize = 4;

    /// The first change of `kind` to a remote-only file: a WRITE, a SETATTR of the size, a
    /// SETATTR of the mode, or a FALLOCATE past its end.
    fn held_change(kind: usize, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        match kind {
            0 => std::fs::OpenOptions::new()
                .write(true)
                .open(path)?
                .write_all_at(EDIT, 0),
            1 => std::fs::OpenOptions::new()
                .write(true)
                .open(path)?
                .set_len(TRUNCATED),
            2 => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)),
            _ => {
                let file = std::fs::OpenOptions::new().write(true).open(path)?;
                let len = (FILE_LEN + ALLOCATED) as libc::off_t;
                // SAFETY: the fd is open for the call.
                if unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len) } < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
        }
    }

    /// Asserts `path` holds what [`held_change`] of `kind` made of `contents`.
    fn assert_held_change(kind: usize, path: &Path, contents: &[u8]) {
        use std::os::unix::fs::PermissionsExt as _;
        let read = std::fs::read(path).expect("read back");
        let expected = match kind {
            0 => {
                let mut expected = contents.to_vec();
                expected[..EDIT.len()].copy_from_slice(EDIT);
                expected
            }
            1 => contents[..TRUNCATED as usize].to_vec(),
            2 => {
                let mode = std::fs::metadata(path).expect("stat").permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{} takes the mode", path.display());
                contents.to_vec()
            }
            _ => {
                let mut expected = contents.to_vec();
                expected.resize(FILE_LEN + ALLOCATED, 0);
                expected
            }
        };
        assert!(
            read == expected,
            "{} holds its change over its content",
            path.display()
        );
    }

    /// How long a request that no worker is left to answer is waited for.
    const ANSWER_WAIT: Duration = Duration::from_secs(5);

    /// `statx` of `path` forcing a request to the mount, whatever the kernel has cached.
    fn statx_forced(path: &Path) -> std::io::Result<()> {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a path");
        // SAFETY: `statx` is plain old data, valid when zeroed.
        let mut out: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and `out` a valid out-pointer.
        let ret = unsafe {
            libc::statx(
                libc::AT_FDCWD,
                path.as_ptr(),
                libc::AT_STATX_FORCE_SYNC,
                libc::STATX_BASIC_STATS,
                &mut out,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// A forced GETATTR of `path` on a thread of its own, answered within [`ANSWER_WAIT`] or
    /// `None`; the thread stays blocked until the mount answers.
    async fn getattr_within(path: PathBuf) -> Option<std::io::Result<()>> {
        let (sender, answered) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = sender.send(statx_forced(&path));
        });
        tokio::time::timeout(ANSWER_WAIT, answered)
            .await
            .ok()
            .map(|sent| sent.expect("the GETATTR thread answers"))
    }

    /// Requests `mount` has answered, of every opcode. A worker counts a request after sending its
    /// reply, so the count can trail an answer the caller has already seen.
    fn answered_requests(mount: &FuseMount) -> usize {
        let metrics = mount.metrics();
        (0..lore_fuse::abi::OPCODE_COUNT as u32)
            .filter_map(|code| Opcode::try_from(code).ok())
            .map(|opcode| metrics.snapshot(opcode).count as usize)
            .sum()
    }

    /// Polls `count` until it reaches `target` or ten seconds pass, and answers the last value.
    async fn wait_for_count(count: impl Fn() -> usize, target: usize) -> usize {
        let waited = Instant::now();
        while count() < target && waited.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copy_ups_held_at_the_remote_leave_every_other_request_served() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("held-copy-ups"), async move {
                let files = HELD * HELD_KINDS;
                let names: Vec<String> = (0..files).map(|i| format!("held{i:02}.bin")).collect();
                let contents: Vec<Vec<u8>> = (0..files)
                    .map(|i| deterministic_bytes(FILE_LEN, 0xc0b1_0000 + 2 * i as u64))
                    .collect();
                let mut served: Vec<(&str, &[u8])> = names
                    .iter()
                    .zip(&contents)
                    .map(|(name, contents)| (name.as_str(), contents.as_slice()))
                    .collect();
                served.push((LOCAL_FILE, b"small"));
                let fixture = remote_fixture_of(&served).await;
                let mut hashes = Vec::new();
                for name in &names {
                    hashes.push(file_address(&fixture, name).await.hash);
                }
                assert_eq!(fixture.store.content_gets(), 0, "the files are remote-only");

                for kind in 0..HELD_KINDS {
                    let round = kind * HELD..(kind + 1) * HELD;
                    fixture.store.hold_hashes(hashes[round.clone()].to_vec());
                    let changes: Vec<_> = names[round.clone()]
                        .iter()
                        .map(|name| {
                            let path = fixture.root.join(name);
                            std::thread::spawn(move || held_change(kind, &path))
                        })
                        .collect();
                    let waiting = wait_for_count(|| fixture.store.waiting(), HELD).await;
                    let before = answered_requests(&fixture.mount);
                    let answered = getattr_within(fixture.root.join(LOCAL_FILE)).await;
                    let served =
                        wait_for_count(|| answered_requests(&fixture.mount), before + 1).await;
                    let still_waiting = fixture.store.waiting();
                    fixture.store.release();
                    answered
                        .unwrap_or_else(|| {
                            panic!(
                                "a GETATTR is answered while every copy-up of kind {kind} \
                                 waits on the remote"
                            )
                        })
                        .expect("statx");
                    assert_eq!(waiting, HELD, "every copy-up of kind {kind} waited at once");
                    assert!(
                        served > before && still_waiting == HELD,
                        "the mount answered the stat while every copy-up of kind {kind} waited: \
                         {before} requests answered before it, {served} after, {still_waiting} \
                         copy-ups waiting"
                    );
                    blocking(move || {
                        for change in changes {
                            change
                                .join()
                                .expect("the change joins")
                                .expect("the first change lands");
                        }
                    })
                    .await;
                    let root = fixture.root.clone();
                    let (names, contents) =
                        (names[round.clone()].to_vec(), contents[round].to_vec());
                    blocking(move || {
                        for (name, contents) in names.iter().zip(&contents) {
                            assert_held_change(kind, &root.join(name), contents);
                        }
                    })
                    .await;
                }
                fixture.unmount().await;
            })
            .await;
    }

    /// A gate a test's listing hook waits on while it is held, counting its waiters.
    #[derive(Default)]
    struct ListingGate {
        held: AtomicBool,
        released: tokio::sync::Notify,
        waiting: AtomicUsize,
    }

    impl ListingGate {
        async fn wait(&self) {
            let mut counted = None;
            loop {
                let released = self.released.notified();
                if !self.held.load(Ordering::SeqCst) {
                    return;
                }
                if counted.is_none() {
                    counted = Some(Waiting::count(&self.waiting));
                }
                released.await;
            }
        }

        fn release(&self) {
            self.held.store(false, Ordering::SeqCst);
            self.released.notify_waiters();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_create_in_a_cold_directory_completes_deferred() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("cold-create"), async move {
                let dirs: Vec<String> = (0..HELD).map(|i| format!("cold{i:02}")).collect();
                let tracked: Vec<String> = dirs
                    .iter()
                    .map(|dir| format!("{dir}/tracked.txt"))
                    .collect();
                let mut files: Vec<(&str, &[u8])> = tracked
                    .iter()
                    .map(|path| (path.as_str(), &b"t"[..]))
                    .collect();
                files.push((LOCAL_FILE, b"small"));
                let fixture = remote_fixture_of(&files).await;

                // The kernel keeps each miss as a negative entry, so an `O_CREAT` open without
                // `O_EXCL` that follows reaches the mount as a CREATE with no LOOKUP before it.
                let (root, names) = (fixture.root.clone(), dirs.clone());
                blocking(move || {
                    for dir in &names {
                        let missing = std::fs::symlink_metadata(root.join(dir).join("new.txt"));
                        assert!(missing.is_err(), "{dir}/new.txt is new");
                    }
                })
                .await;

                // Each directory goes cold, and building its listing waits until released.
                let gate = Arc::new(ListingGate::default());
                gate.held.store(true, Ordering::SeqCst);
                let hooked = gate.clone();
                fixture
                    .mount
                    .filesystem
                    .set_listing_hook(Arc::new(move |node| {
                        let gate = hooked.clone();
                        Box::pin(async move {
                            if node != ROOT_NODE {
                                gate.wait().await;
                            }
                        })
                    }));
                fixture.mount.filesystem.served().listings.clear();

                let creators: Vec<_> = dirs
                    .iter()
                    .map(|dir| {
                        let path = fixture.root.join(dir).join("new.txt");
                        std::thread::spawn(move || {
                            use std::io::Write as _;
                            std::fs::OpenOptions::new()
                                .write(true)
                                .create(true)
                                .truncate(false)
                                .open(&path)?
                                .write_all(b"created")
                        })
                    })
                    .collect();
                let waiting = wait_for_count(|| gate.waiting.load(Ordering::SeqCst), HELD).await;
                let answered = getattr_within(fixture.root.join(LOCAL_FILE)).await;
                gate.release();
                answered
                    .expect("a GETATTR is answered while every CREATE waits on its directory")
                    .expect("statx");
                assert_eq!(
                    waiting, HELD,
                    "every CREATE waited on its directory at once"
                );

                blocking(move || {
                    for creator in creators {
                        creator
                            .join()
                            .expect("the creator joins")
                            .expect("the create completes");
                    }
                })
                .await;
                let root = fixture.root.clone();
                blocking(move || {
                    for dir in &dirs {
                        let mut names: Vec<String> = std::fs::read_dir(root.join(dir))
                            .expect("listing")
                            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
                            .collect();
                        names.sort();
                        assert_eq!(names, vec!["new.txt", "tracked.txt"], "{dir}");
                        let created = std::fs::read(root.join(dir).join("new.txt"));
                        assert_eq!(created.expect("read back"), b"created", "{dir}");
                    }
                })
                .await;
                fixture.unmount().await;
            })
            .await;
    }

    // ---------------------------------------------------------------------------------------------
    // Clone-time prefetch: the tree to a depth, every block, and an access recording
    // ---------------------------------------------------------------------------------------------

    use std::collections::BTreeSet;

    use lore_revision::fs::fuse::filesystem::FuseFilesystem;
    use lore_revision::node::NodeBlock;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::RepositoryAccess;
    use lore_revision::repository::VIEW_FILTER;
    use lore_revision::repository::clone::CloneContext;
    use lore_revision::repository::clone::CloneOptions;
    use lore_revision::repository::clone::clone_materialize_hold;
    use lore_revision::repository::clone::clone_onto_held_state;
    use lore_revision::repository::prefetch::TreePrefetch;
    use lore_revision::state::State;
    use lore_revision::state::StateNodeChildrenWithNameIterator;

    /// Files in each of the two large directories of [`prefetch_tree`]: enough that each fills
    /// node blocks no other directory's children share.
    const SPREAD: usize = 1200;

    /// A view excluding the `skip` directory and everything below it.
    const VIEW_WITHOUT_SKIP: &str = "/skip/\n";

    const PREFETCH_WAIT: Duration = Duration::from_secs(120);

    /// Three commits, so that each group of nodes is allocated after the one before: `top.txt`,
    /// `keep/k1.txt` and `keep/sub/k2.txt`, then `SPREAD` files in `skip/` (depth 2), then
    /// `SPREAD` files in `keep/sub/deep/` (depth 4).
    fn prefetch_tree() -> Vec<Vec<(String, Vec<u8>)>> {
        vec![
            vec![
                ("top.txt".to_string(), b"top".to_vec()),
                ("keep/k1.txt".to_string(), b"k1".to_vec()),
                ("keep/sub/k2.txt".to_string(), b"k2".to_vec()),
            ],
            (0..SPREAD)
                .map(|i| {
                    (
                        format!("skip/s{i:04}.txt"),
                        format!("skip {i}").into_bytes(),
                    )
                })
                .collect(),
            (0..SPREAD)
                .map(|i| {
                    (
                        format!("keep/sub/deep/d{i:04}.txt"),
                        format!("deep {i}").into_bytes(),
                    )
                })
                .collect(),
        ]
    }

    /// The tree depth the level-bounded prefetch tests warm.
    const TWO_LEVELS: std::num::NonZeroU32 = std::num::NonZeroU32::new(2).unwrap();

    fn prefetch_options(tree: TreePrefetch, recording: Option<&Path>) -> CloneOptions {
        CloneOptions {
            tree_prefetch: Some(tree),
            prefetch: recording.map(|path| path.display().to_string()),
            ..Default::default()
        }
    }

    /// A virtual clone of a served tree as the service makes one, up to its publish: a FUSE
    /// instance created over an empty shared store and mounted empty, loaded with its remote
    /// and view, with the served revision's state read through it.
    struct VfsClone {
        // Field order is drop order: unmount before the directories go.
        context: CloneContext,
        filesystem: Arc<FuseFilesystem>,
        manager: Arc<MountManager>,
        _guard: UnmountGuard,
        root: PathBuf,
        _dir: TempDir,
        _store_dir: TempDir,
        store: Arc<CountingStore>,
        _server: Server,
        /// The committed tree, read from the server's own stores rather than through it.
        served_repository: Arc<RepositoryContext>,
        served_state: Arc<State>,
    }

    async fn vfs_clone(
        steps: &[Vec<(String, Vec<u8>)>],
        view: Option<&str>,
        options: CloneOptions,
    ) -> VfsClone {
        let (backend_immutable, backend_mutable) = memory_stores().await;
        let store = Arc::new(CountingStore::new(backend_immutable.clone()));
        let served: Arc<dyn ImmutableStore> = store.clone();
        let server = start_server(served, backend_mutable.clone()).await;

        let id = RepositoryId::from(uuid::Uuid::now_v7());
        let borrowed: Vec<Vec<(&str, &[u8])>> = steps
            .iter()
            .map(|files| {
                files
                    .iter()
                    .map(|(name, contents)| (name.as_str(), contents.as_slice()))
                    .collect()
            })
            .collect();
        let borrowed: Vec<&[(&str, &[u8])]> = borrowed.iter().map(Vec::as_slice).collect();
        let revision = commit_steps_to_server(
            backend_immutable.clone(),
            backend_mutable.clone(),
            id,
            &borrowed,
        )
        .await;
        let served_repository = Arc::new(RepositoryContext::new(creation_args(
            backend_immutable,
            backend_mutable,
            id,
            InstanceId::generate(),
            None,
            Err(lore_transport::ProtocolError::from(
                lore_base::error::NoRemote,
            )),
        )));
        let served_state = State::deserialize(served_repository.clone(), revision)
            .await
            .expect("the committed state reads from the server's store");

        let manager = mount_manager().await;
        let store_dir = TempDir::new("lore-fuse-prefetch-store-");
        let dir = TempDir::new("lore-fuse-prefetch-mount-");
        let root = std::fs::canonicalize(dir.path()).expect("canonical mount point");
        let guard = UnmountGuard(root.clone());
        let config = RepositoryConfig {
            remote_url: Some(server.url.clone()),
            shared_store_to_use: Some(SharedStoreToUseConfig {
                use_shared_store: Some(true),
                shared_store_path: Some(store_dir.path().display().to_string()),
            }),
            vfs: Some(VfsConfig {
                vfs_type: VfsType::Fuse,
                base_time: Some(BASE_TIME),
            }),
            ..Default::default()
        };
        let token = RepositoryWriteToken::acquire(&root).await;
        let created = lore_revision::repository::create_local(
            &root,
            &token,
            id,
            lore_base::types::Context::from(uuid::Uuid::now_v7()),
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            config,
            false,
        )
        .await
        .expect("create_local mounts the FUSE instance");
        if let Some(view) = view {
            let dot_lore = external_dir_for_instance(created.instance_id)
                .expect("external dir resolves")
                .join(DOT_LORE);
            // Fixture write of the instance's view, as clone copies it.
            #[allow(clippy::disallowed_methods)]
            std::fs::write(dot_lore.join(VIEW_FILTER), view).expect("view file");
        }
        drop(created);
        let repository = lore_revision::repository::load_and_connect_with_token(
            &root,
            RepositoryAccess::ReadWrite,
            Some(token.share()),
        )
        .await
        .expect("the instance loads with its remote");
        drop(token);
        let state = State::deserialize(repository.clone(), revision)
            .await
            .expect("the revision's state fetches");
        state
            .tree(repository.clone())
            .await
            .expect("the revision's tree header fetches");
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("operation");
        assert!(operation.holds_state(), "a FUSE instance publishes states");
        let filesystem = manager
            .fuse()
            .mounted(&root)
            .expect("the created instance is mounted")
            .filesystem
            .clone();
        store.take_served();

        VfsClone {
            context: CloneContext {
                repository,
                state,
                operation,
                options: Arc::new(options),
                stats: Arc::default(),
                modified_times: Arc::default(),
            },
            filesystem,
            manager,
            _guard: guard,
            root,
            _dir: dir,
            _store_dir: store_dir,
            store,
            _server: server,
            served_repository,
            served_state,
        }
    }

    impl VfsClone {
        /// Publishes the revision as a clone does and waits for the prefetch it starts.
        async fn publish_and_prefetch(&self) {
            clone_materialize_hold(&self.context)
                .await
                .expect("the clone publishes");
            tokio::time::timeout(PREFETCH_WAIT, self.filesystem.prefetch_finished())
                .await
                .expect("the prefetch finishes");
        }

        /// The clone's local store as a context that cannot reach the remote, with the
        /// revision's state read through it: a read succeeds only for what the store holds.
        async fn offline(&self) -> (Arc<RepositoryContext>, Arc<State>) {
            let (_, mutable) = memory_stores().await;
            let repository = Arc::new(RepositoryContext::new(creation_args(
                self.context.repository.immutable_store(),
                mutable,
                self.context.repository.id,
                InstanceId::generate(),
                None,
                Err(lore_transport::ProtocolError::from(
                    lore_base::error::NoRemote,
                )),
            )));
            let state = State::deserialize(repository.clone(), self.served_state.revision())
                .await
                .expect("the state is local");
            (repository, state)
        }

        /// The address hash of every node block, by block index.
        async fn node_block_hashes(&self) -> Vec<lore_base::types::Hash> {
            let (nodes, _) = self
                .served_state
                .block_address_lists(self.served_repository.clone())
                .await
                .expect("the served block lists read");
            lore_base::types::TypedBytes::as_type_slice::<lore_base::types::Hash>(&nodes).to_vec()
        }

        /// Asserts every block in `blocks` is local with its name table: a listing reading any
        /// of them needs no fetch.
        async fn assert_listable(&self, blocks: &BTreeSet<usize>) {
            let (repository, state) = self.offline().await;
            for &block in blocks {
                state
                    .block_with_nametable(repository.clone(), block)
                    .await
                    .unwrap_or_else(|error| panic!("block {block} and its name table: {error}"));
            }
        }

        /// Asserts every block in `blocks` is local with its name table and file-metadata block,
        /// as a block warm-up leaves it.
        async fn assert_warmed(&self, blocks: &BTreeSet<usize>) {
            self.assert_listable(blocks).await;
            let (repository, state) = self.offline().await;
            for &block in blocks {
                state
                    .block_file_metadata(repository.clone(), block)
                    .await
                    .unwrap_or_else(|error| panic!("block {block}'s file metadata: {error}"));
            }
        }

        /// Asserts no block in `blocks` was fetched or is local, given what the server served.
        async fn assert_untouched(
            &self,
            blocks: &BTreeSet<usize>,
            served: &[(lore_base::types::Address, lore_base::types::Fragment)],
        ) {
            let hashes = self.node_block_hashes().await;
            let (repository, state) = self.offline().await;
            for &block in blocks {
                let hash = hashes[block];
                assert!(
                    !served.iter().any(|(address, _)| address.hash == hash),
                    "block {block} was fetched"
                );
                assert!(
                    state.block(repository.clone(), block).await.is_err(),
                    "block {block} is local"
                );
            }
        }

        /// Whether the content of `path` in the served revision is in the clone's local store.
        async fn content_local(&self, path: &str) -> bool {
            let node = self
                .served_state
                .find_node(self.served_repository.clone(), path)
                .await
                .unwrap_or_else(|error| panic!("{path} is in the served tree: {error}"));
            let repository = &self.context.repository;
            repository
                .immutable_store()
                .get(repository.id, node.address)
                .await
                .is_ok_and(|data| data.payload.is_some())
        }

        async fn unmount(self) {
            unmount_through(&self.manager, &self.root)
                .await
                .expect("unmount");
        }
    }

    /// A node of the committed tree, placed by the block holding it.
    struct Placed {
        block: usize,
        /// The directory holding it.
        parent: String,
        /// Levels below the root of the directory holding it.
        parent_depth: u32,
        /// Whether the view keeps every directory between the root and it.
        kept_above: bool,
    }

    /// Every node of `state` below its root, placed, with `excluded` naming the directories
    /// the view excludes.
    async fn place_nodes(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        excluded: &[&str],
    ) -> Vec<Placed> {
        let mut placed = Vec::new();
        let mut directories = vec![(ROOT_NODE, 0u32, String::new(), true)];
        while let Some((node, depth, path, kept)) = directories.pop() {
            let mut children =
                StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node)
                    .await
                    .expect("children open");
            while let Some((id, child, name)) = children.next().await.expect("child reads") {
                let name = (*name).to_string();
                let child_path = if path.is_empty() {
                    name
                } else {
                    format!("{path}/{name}")
                };
                placed.push(Placed {
                    block: NodeBlock::index(id),
                    parent: path.clone(),
                    parent_depth: depth,
                    kept_above: kept,
                });
                if child.is_directory() {
                    let child_kept = kept && !excluded.contains(&child_path.as_str());
                    directories.push((id, depth + 1, child_path, child_kept));
                }
            }
        }
        placed
    }

    /// The blocks a walk `levels` deep loads with their name tables: every block holding a child
    /// of a directory the walk enters.
    fn walked_blocks(placed: &[Placed], levels: u32) -> BTreeSet<usize> {
        placed
            .iter()
            .filter(|node| node.kept_above && node.parent_depth < levels)
            .map(|node| node.block)
            .collect()
    }

    fn all_blocks(placed: &[Placed]) -> BTreeSet<usize> {
        placed.iter().map(|node| node.block).collect()
    }

    /// The blocks holding a child of `directory`.
    fn blocks_listing(placed: &[Placed], directory: &str) -> BTreeSet<usize> {
        placed
            .iter()
            .filter(|node| node.parent == directory)
            .map(|node| node.block)
            .collect()
    }

    /// The blocks holding children of `directory` and nothing else.
    fn blocks_only_listing(placed: &[Placed], directory: &str) -> BTreeSet<usize> {
        let others: BTreeSet<usize> = placed
            .iter()
            .filter(|node| node.parent != directory)
            .map(|node| node.block)
            .collect();
        blocks_listing(placed, directory)
            .difference(&others)
            .copied()
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tree_prefetch_warms_every_block_to_its_depth_and_nothing_deeper() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-levels"), async move {
                let clone = vfs_clone(
                    &prefetch_tree(),
                    None,
                    prefetch_options(TreePrefetch::Levels(TWO_LEVELS), None),
                )
                .await;
                clone.publish_and_prefetch().await;
                let served = clone.store.take_served();

                let placed = place_nodes(&clone.served_repository, &clone.served_state, &[]).await;
                let walked = walked_blocks(&placed, 2);
                let deeper: BTreeSet<usize> =
                    all_blocks(&placed).difference(&walked).copied().collect();
                assert!(
                    walked.len() > 1 && !deeper.is_empty(),
                    "the fixture spreads its levels over blocks: {walked:?} then {deeper:?}"
                );
                clone.assert_listable(&walked).await;
                clone.assert_untouched(&deeper, &served).await;
                clone.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tree_prefetch_skips_view_excluded_subtrees() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-view"), async move {
                let clone = vfs_clone(
                    &prefetch_tree(),
                    Some(VIEW_WITHOUT_SKIP),
                    prefetch_options(TreePrefetch::Levels(TWO_LEVELS), None),
                )
                .await;
                clone.publish_and_prefetch().await;
                let served = clone.store.take_served();

                let placed =
                    place_nodes(&clone.served_repository, &clone.served_state, &["skip"]).await;
                let walked = walked_blocks(&placed, 2);
                let skipped = blocks_only_listing(&placed, "skip");
                assert!(
                    !skipped.is_empty(),
                    "the fixture gives the excluded directory blocks of its own"
                );
                clone.assert_listable(&walked).await;
                clone.assert_untouched(&skipped, &served).await;
                clone.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn whole_tree_prefetch_walks_every_directory_the_view_keeps() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-whole-view"), async move {
                let clone = vfs_clone(
                    &prefetch_tree(),
                    Some(VIEW_WITHOUT_SKIP),
                    prefetch_options(TreePrefetch::View, None),
                )
                .await;
                clone.publish_and_prefetch().await;
                let served = clone.store.take_served();

                let placed =
                    place_nodes(&clone.served_repository, &clone.served_state, &["skip"]).await;
                let walked = walked_blocks(&placed, u32::MAX);
                assert!(
                    !blocks_listing(&placed, "keep/sub/deep").is_disjoint(&walked),
                    "the deepest directory is walked"
                );
                clone.assert_listable(&walked).await;
                clone
                    .assert_untouched(&blocks_only_listing(&placed, "skip"), &served)
                    .await;
                clone.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn block_prefetch_warms_every_block_whatever_the_view() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-blocks"), async move {
                let clone = vfs_clone(
                    &prefetch_tree(),
                    Some(VIEW_WITHOUT_SKIP),
                    prefetch_options(TreePrefetch::Blocks, None),
                )
                .await;
                clone.publish_and_prefetch().await;

                let every: BTreeSet<usize> = clone
                    .node_block_hashes()
                    .await
                    .iter()
                    .enumerate()
                    .filter(|(_, hash)| !hash.is_zero())
                    .map(|(block, _)| block)
                    .collect();
                assert!(every.len() > 3, "the tree spans blocks: {every:?}");
                clone.assert_warmed(&every).await;
                clone.unmount().await;
            })
            .await;
    }

    /// A recording as the access recorder writes one: the root, directories with a trailing
    /// `/`, first-access order, a repeat, a path the revision lacks, and paths the view
    /// excludes.
    const RECORDING: &str = "/\nkeep/\nkeep/sub/k2.txt\nkeep/k1.txt\nkeep/sub/k2.txt\n\
                             missing/nothing.txt\nskip/s0001.txt\nskip/\nkeep/sub/deep/\n";

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recording_prefetch_warms_recorded_listings_and_contents_and_skips_the_rest() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-recording"), async move {
                let recording = lore_base::test_util::TempFile::with_contents(
                    "lore-fuse-recording-",
                    RECORDING.as_bytes(),
                );
                let clone = vfs_clone(
                    &prefetch_tree(),
                    Some(VIEW_WITHOUT_SKIP),
                    prefetch_options(TreePrefetch::Off, Some(recording.path())),
                )
                .await;
                clone.publish_and_prefetch().await;
                let served = clone.store.take_served();

                let placed =
                    place_nodes(&clone.served_repository, &clone.served_state, &["skip"]).await;
                for directory in ["", "keep", "keep/sub", "keep/sub/deep"] {
                    clone
                        .assert_listable(&blocks_listing(&placed, directory))
                        .await;
                }
                for file in ["keep/k1.txt", "keep/sub/k2.txt"] {
                    assert!(clone.content_local(file).await, "{file} is warmed");
                }
                assert!(
                    !clone.content_local("keep/sub/deep/d0001.txt").await,
                    "a listed directory's files are not"
                );
                assert!(
                    !clone.content_local("skip/s0001.txt").await,
                    "a file the view excludes is not"
                );
                let skipped = blocks_only_listing(&placed, "skip");
                assert!(
                    !skipped.is_empty(),
                    "the excluded directory has blocks of its own"
                );
                clone.assert_untouched(&skipped, &served).await;
                clone.unmount().await;
            })
            .await;
    }

    /// More recorded files than fetch at once, so resolved files wait for a content fetch while
    /// paths still resolve.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recording_prefetch_warms_every_file_of_a_long_recording() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-long-recording"), async move {
                let files: Vec<String> = (0..SPREAD)
                    .map(|i| format!("keep/sub/deep/d{i:04}.txt"))
                    .collect();
                let mut text = files.join("\n");
                text.push('\n');
                let recording = lore_base::test_util::TempFile::with_contents(
                    "lore-fuse-recording-",
                    text.as_bytes(),
                );
                let clone = vfs_clone(
                    &prefetch_tree(),
                    None,
                    prefetch_options(TreePrefetch::Off, Some(recording.path())),
                )
                .await;
                clone.publish_and_prefetch().await;

                for file in &files {
                    assert!(clone.content_local(file).await, "{file} is warmed");
                }
                clone.unmount().await;
            })
            .await;
    }

    /// A recorded file's content arrives while a directory recorded before it still waits on
    /// the remote.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recording_prefetch_fetches_a_resolved_file_while_earlier_paths_resolve() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-recording-stream"), async move {
                let recording = lore_base::test_util::TempFile::with_contents(
                    "lore-fuse-recording-",
                    b"keep/sub/deep/\nkeep/k1.txt\n",
                );
                let clone = vfs_clone(
                    &prefetch_tree(),
                    None,
                    prefetch_options(TreePrefetch::Off, Some(recording.path())),
                )
                .await;
                let placed = place_nodes(&clone.served_repository, &clone.served_state, &[]).await;
                let hashes = clone.node_block_hashes().await;
                let deep = blocks_only_listing(&placed, "keep/sub/deep");
                assert!(!deep.is_empty(), "the deep directory has blocks of its own");
                clone
                    .store
                    .hold_hashes(deep.iter().map(|&block| hashes[block]).collect());
                clone_materialize_hold(&clone.context)
                    .await
                    .expect("the clone publishes");
                tokio::time::timeout(Duration::from_secs(20), async {
                    while !clone.content_local("keep/k1.txt").await {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("keep/k1.txt is warmed while keep/sub/deep/ waits on the remote");
                assert!(
                    clone.filesystem.prefetch_running(),
                    "keep/sub/deep/ still resolves"
                );
                clone.store.release();
                tokio::time::timeout(PREFETCH_WAIT, clone.filesystem.prefetch_finished())
                    .await
                    .expect("the prefetch finishes");
                clone.unmount().await;
            })
            .await;
    }

    /// A recorded directory's block hints warm the blocks they name, even those of a directory
    /// the view excludes, which resolving the recorded path does not reach.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recording_block_hints_warm_the_blocks_they_name() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-block-hints"), async move {
                let recording =
                    lore_base::test_util::TempFile::with_contents("lore-fuse-recording-", b"");
                let clone = vfs_clone(
                    &prefetch_tree(),
                    Some(VIEW_WITHOUT_SKIP),
                    prefetch_options(TreePrefetch::Off, Some(recording.path())),
                )
                .await;
                let placed =
                    place_nodes(&clone.served_repository, &clone.served_state, &["skip"]).await;
                let skipped = blocks_only_listing(&placed, "skip");
                assert!(
                    !skipped.is_empty(),
                    "the excluded directory has blocks of its own"
                );
                let hints: Vec<String> = skipped.iter().map(usize::to_string).collect();
                // Fixture write of the recording, which the clone reads when it publishes.
                #[allow(clippy::disallowed_methods)]
                std::fs::write(recording.path(), format!("skip/\t{}\n", hints.join(" ")))
                    .expect("recording");
                clone.publish_and_prefetch().await;
                clone.assert_warmed(&skipped).await;
                clone.unmount().await;
            })
            .await;
    }

    /// A clone's hold reads none of the root's ignore file, and the warm-up after it reads the file
    /// into the store, so a command run offline finds it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_warm_up_after_a_hold_reads_the_ignore_file_into_the_store() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("held-ignore-file"), async move {
                let tree = vec![vec![
                    (".loreignore".to_string(), b"*.held-warm-up\n".to_vec()),
                    ("top.txt".to_string(), b"top".to_vec()),
                ]];
                let clone = vfs_clone(&tree, None, prefetch_options(TreePrefetch::Off, None)).await;
                clone_materialize_hold(&clone.context)
                    .await
                    .expect("the clone publishes");
                assert!(
                    !clone.content_local(".loreignore").await,
                    "the hold reads the ignore file"
                );
                clone.context.operation.warm_held_ignore_file().await;
                assert!(
                    clone.content_local(".loreignore").await,
                    "the warm-up reads the ignore file into the store"
                );
                clone.unmount().await;
            })
            .await;
    }

    /// A clone onto a mount starts its prefetch before it reads the root's ignore file, and returns
    /// once that read has stored the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_clone_starts_its_prefetch_before_it_reads_the_ignore_file() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("held-clone-ignore-file"), async move {
                let recording = lore_base::test_util::TempFile::with_contents(
                    "lore-fuse-recording-",
                    b"keep/held-clone.txt\n",
                );
                let mut tree = prefetch_tree();
                tree[0].push((".loreignore".to_string(), b"*.held-clone\n".to_vec()));
                tree[0].push((
                    "keep/held-clone.txt".to_string(),
                    b"held by the clone order test".to_vec(),
                ));
                let clone = vfs_clone(
                    &tree,
                    None,
                    prefetch_options(TreePrefetch::Off, Some(recording.path())),
                )
                .await;
                let mut held = Vec::new();
                for path in [".loreignore", "keep/held-clone.txt"] {
                    let node = clone
                        .served_state
                        .find_node(clone.served_repository.clone(), path)
                        .await
                        .expect("a served file");
                    held.push(node.address.hash);
                }
                let ignore_file = held[0];
                clone.store.hold_hashes(held);
                let cache_task = {
                    let mut cloned = std::pin::pin!(clone_onto_held_state(&clone.context));
                    let asked = async {
                        while !clone.store.has_arrived(&ignore_file) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = &mut cloned => panic!("the clone returns before the ignore file is stored"),
                        asked = tokio::time::timeout(Duration::from_secs(10), asked) => {
                            asked.expect("the clone asks the remote for the ignore file");
                        }
                    }
                    assert!(
                        clone.filesystem.prefetch_running(),
                        "the prefetch starts before the ignore file is read"
                    );
                    clone.store.release();
                    tokio::time::timeout(Duration::from_secs(10), cloned)
                        .await
                        .expect("the clone returns once the remote answers")
                        .expect("the clone publishes")
                        .expect("the clone caches the state's records")
                };
                cache_task
                    .await
                    .expect("the caching task")
                    .expect("the state's records cache");
                assert!(
                    clone.content_local(".loreignore").await,
                    "the ignore file is stored"
                );
                tokio::time::timeout(PREFETCH_WAIT, clone.filesystem.prefetch_finished())
                    .await
                    .expect("the prefetch finishes once the remote answers");
                clone.unmount().await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clone_returns_before_its_prefetch_completes() {
        skip_without_fuse!();
        LORE_CONTEXT
            .scope(execution("prefetch-background"), async move {
                let recording = lore_base::test_util::TempFile::with_contents(
                    "lore-fuse-recording-",
                    b"keep/k1.txt\n",
                );
                let clone = vfs_clone(
                    &prefetch_tree(),
                    None,
                    prefetch_options(TreePrefetch::Off, Some(recording.path())),
                )
                .await;
                clone.store.hold();
                tokio::time::timeout(
                    Duration::from_secs(10),
                    clone_materialize_hold(&clone.context),
                )
                .await
                .expect("the clone returns while the remote holds every fetch")
                .expect("the clone publishes");
                assert!(
                    clone.filesystem.prefetch_running(),
                    "the prefetch waits on the remote"
                );
                assert!(!clone.content_local("keep/k1.txt").await);

                clone.store.release();
                tokio::time::timeout(PREFETCH_WAIT, clone.filesystem.prefetch_finished())
                    .await
                    .expect("the prefetch finishes once the remote answers");
                assert!(!clone.filesystem.prefetch_running());
                assert!(
                    clone.content_local("keep/k1.txt").await,
                    "the recorded file is warmed"
                );
                clone.unmount().await;
            })
            .await;
    }
}
