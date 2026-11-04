// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod remote_store_tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::fragment;
    use lore_revision::lore::RepositoryId;
    use lore_revision::store::remote::RemoteImmutableStore;
    use lore_revision::store::remote::RemoteMutableStore;
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
    use lore_storage::CallerBuffer;
    use lore_storage::CompressionMode;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_storage::PayloadRead;
    use lore_storage::ReadOptions;
    use lore_storage::StoreGetData;
    use lore_storage::StoreMatch;
    use lore_storage::StoreMatchResult;
    use lore_storage::immutable_store::CopyBehavior;
    use lore_storage::immutable_store::query_one;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use rand::random;

    use crate::common::net_common::bind_matched_pair;
    use crate::common::net_common::cached_connection;
    use crate::common::net_common::wait_out_connect_back_off;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    struct TestServer {
        immutable_store: Arc<RemoteImmutableStore>,
        mutable_store: Arc<RemoteMutableStore>,
        /// The store behind the server, written directly to plant what the protocol would refuse.
        backend: Arc<dyn ImmutableStore>,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// The stores a real server runs, behind its gRPC services. Both harnesses build on this; the
    /// QUIC one adds a storage endpoint on the same address.
    async fn start_backend(
        listener: std::net::TcpListener,
        addr: SocketAddr,
    ) -> (
        Arc<dyn ImmutableStore>,
        Arc<dyn MutableStore>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let backend_immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                implicit_durable_stored: true,
                // As a real server runs it. One process holds content for every tenant, so it
                // serves the association it was asked for and nothing else.
                isolate_partitions: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let backend_mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            backend_immutable.clone(),
        )
        .await
        .unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let signal = async {
            shutdown_rx.await.ok();
        };

        let notification_sender: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());
        let hook_dispatcher = Arc::new(HookDispatcher::empty());

        let (stopped_tx, mut stopped_rx) = tokio::sync::oneshot::channel::<String>();
        let served_immutable = backend_immutable.clone();
        let served_mutable = backend_mutable.clone();
        // Background server task in a test; LORE_CONTEXT propagation is unnecessary here.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let repository_catalog = Arc::new(BaselineRepositoryCatalog::new(
                BaselineAccess::Reachable,
                served_immutable.clone(),
                served_mutable.clone(),
            ));
            let outcome = GrpcServerBuilder::new()
                .with_environment(EnvironmentConfig::default())
                .with_feature(FeatureSettings::default())
                .with_immutable_store(served_immutable.clone(), served_immutable)
                .with_mutable_store(served_mutable)
                .with_lock_store(None)
                .with_notification(notification_sender, None)
                .with_hook_dispatcher(hook_dispatcher)
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
                    repository_catalog,
                )
                .unwrap()
                .serve_with_listener(listener, signal)
                .await;
            let _ = stopped_tx.send(match outcome {
                Ok(()) => "stopped before the test finished".to_string(),
                Err(error) => format!("failed: {error}"),
            });
        });

        // A server that never starts must say so. Falling through to a client that can never be
        // answered is what turns a startup failure into a test that hangs with nothing on stderr.
        let mut ready = false;
        for _ in 0..50 {
            if let Ok(reason) = stopped_rx.try_recv() {
                panic!("test server on {addr} {reason}");
            }
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "test server on {addr} never accepted a connection");

        (backend_immutable, backend_mutable, shutdown_tx)
    }

    async fn start_test_server() -> TestServer {
        // gRPC only, so no UDP half is needed: bind TCP and hand it straight over.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (backend, _mutable, shutdown_tx) = start_backend(listener, addr).await;

        let url = format!("grpc://127.0.0.1:{}", addr.port());
        TestServer {
            immutable_store: Arc::new(RemoteImmutableStore::new(&url, None)),
            mutable_store: Arc::new(RemoteMutableStore::new(&url, None)),
            backend,
            _shutdown: shutdown_tx,
        }
    }

    struct QuicTestServer {
        url: String,
        immutable_store: Arc<RemoteImmutableStore>,
        _server: QuinnServer,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// The same stores, with reads and writes carried over QUIC instead of gRPC. A `lore://`
    /// connection still resolves its environment over gRPC, so both endpoints share one address:
    /// gRPC on TCP, storage on UDP.
    ///
    /// The gRPC storage transport multiplexes every read of a session onto one bidirectional
    /// stream and reports a missing fragment as that stream's terminal status, so one miss ends
    /// the stream and every later read on that session waits forever. QUIC frames each command
    /// separately and has no such coupling, so the contract battery runs here until that is fixed.
    async fn start_test_quic_server() -> QuicTestServer {
        // Both halves of the port are held before either server starts, so neither can lose it to
        // something else between being told the number and binding it.
        let (listener, udp) = bind_matched_pair();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (backend_immutable, backend_mutable, shutdown_tx) = start_backend(listener, addr).await;

        let (cert_file, pkey_file, _ca) =
            lore_server::quic::tests::server_certs().expect("test certificate paths");

        let server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .socket(udp)
                .cert_file(cert_file)
                .pkey_file(pkey_file)
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    backend_immutable,
                    backend_mutable,
                )))
                .build()
                .expect("quinn config"),
        )
        .expect("quinn server start");

        // Plain `lore` rather than `lores`, so the client skips verification of the self-signed
        // test certificate.
        let url = format!("lore://127.0.0.1:{}", addr.port());
        QuicTestServer {
            immutable_store: Arc::new(RemoteImmutableStore::new(&url, None)),
            url,
            _server: server,
            _shutdown: shutdown_tx,
        }
    }

    // ── Immutable Store Tests ──

    /// A read that misses must not stop the reads after it. This is the shape that wedges the
    /// gRPC transport, where a miss is the terminal status of the stream every read shares.
    #[tokio::test]
    async fn a_missing_read_does_not_stop_the_next_one() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_quic_server().await;
                let store = server.immutable_store.clone();
                let partition = lore_base::types::Partition::from(rand::random::<[u8; 16]>());
                let payload = bytes::Bytes::from_static(b"read me after a miss");
                let fragment = lore_base::types::Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                let stored = lore_base::types::Address {
                    hash: lore_storage::hash::hash_slice(payload.as_ref()),
                    context: lore_base::types::Context::from(rand::random::<[u8; 16]>()),
                };
                store
                    .clone()
                    .put(partition, stored, fragment, Some(payload.clone()), false)
                    .await
                    .expect("put should succeed");

                let missing = lore_base::types::Address {
                    hash: rand::random(),
                    context: lore_base::types::Context::from(rand::random::<[u8; 16]>()),
                };
                assert!(store.clone().get(partition, missing).await.is_err());

                let (_fragment, bytes) = store
                    .clone()
                    .get(partition, stored)
                    .await
                    .and_then(StoreGetData::into_payload)
                    .expect("a read after a miss should still be answered");
                assert_eq!(bytes.as_ref(), payload.as_ref());
                Ok(())
            })
            .await
    }

    /// A remote store replaces a connection the transport has marked stale once its connect's
    /// back-off has passed, as the QUIC client marks one once it gives up reconnecting, and the
    /// replaced connection is freed though a session pool it pinned held it. The test marks it by
    /// hand.
    #[tokio::test]
    async fn a_remote_store_replaces_and_frees_a_stale_connection() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_quic_server().await;
                let store = server.immutable_store.clone();
                let partition = lore_base::types::Partition::from(rand::random::<[u8; 16]>());
                let payload = bytes::Bytes::from_static(b"served over the replacement");
                let fragment = lore_base::types::Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                let address = lore_base::types::Address {
                    hash: lore_storage::hash::hash_slice(payload.as_ref()),
                    context: lore_base::types::Context::from(rand::random::<[u8; 16]>()),
                };
                store
                    .clone()
                    .put(partition, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("put should succeed");
                wait_out_connect_back_off().await;
                let stale = {
                    let connection = cached_connection(&server.url)
                        .expect("the store must have connected through the connection cache");
                    connection
                        .stale
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Arc::downgrade(&connection)
                };

                let (_fragment, bytes) = store
                    .clone()
                    .get(partition, address)
                    .await
                    .and_then(StoreGetData::into_payload)
                    .expect("a get after the connection went stale should be served");

                assert_eq!(bytes.as_ref(), payload.as_ref());
                assert!(
                    stale.upgrade().is_none(),
                    "the replaced connection must be freed"
                );
                Ok(())
            })
            .await
    }

    /// The contract, checked against a store on the other end of a wire.
    ///
    /// The clauses bind a protocol handler and the client store that decodes its answers just as
    /// they bind a store called in process - together they are one implementation spanning a
    /// connection. The server is configured the way a real one is, isolating partitions, because
    /// the pair satisfies the client store's declared read scope only if the server it talks to
    /// answers exact associations. The wire collapse of a hash-only match is covered where the
    /// mapping lives, in `lore-server`'s query handler, against a store that does report one.
    #[tokio::test]
    async fn remote_store_satisfies_the_immutable_store_contract() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;

                lore_storage::conformance::verify_immutable_store(
                    server.immutable_store.clone(),
                    lore_storage::conformance::Capabilities::new("RemoteImmutableStore/grpc")
                        .over_wire()
                        .miss_poisons_session(),
                )
                .await;

                Ok(())
            })
            .await
    }

    /// The same contract over QUIC, which frames each command separately and so carries the
    /// checks the gRPC transport cannot answer. This is the run that covers reading after a miss.
    #[tokio::test]
    async fn quic_remote_store_satisfies_the_immutable_store_contract() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_quic_server().await;

                lore_storage::conformance::verify_immutable_store(
                    server.immutable_store.clone(),
                    lore_storage::conformance::Capabilities::new("RemoteImmutableStore/quic")
                        .over_wire(),
                )
                .await;

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_put_and_get() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (fragment, address, payload) = fragment::generate_random();

                server
                    .immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let (got_fragment, got_payload) = server
                    .immutable_store
                    .clone()
                    .get(repository, address)
                    .await
                    .and_then(StoreGetData::into_payload)?;

                assert_eq!(payload, got_payload);
                assert_eq!(fragment.size_payload, got_fragment.size_payload);
                assert_eq!(fragment.size_content, got_fragment.size_content);

                Ok(())
            })
            .await
    }

    /// A durably stored local payload in an encoding this build no longer decodes is replaced by
    /// the remote's copy on the first read, even one that does not ask to cache, so later reads
    /// are answered locally.
    #[tokio::test]
    async fn an_undecodable_durable_local_payload_is_replaced_from_the_remote() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (fragment, address, payload) = fragment::generate_random();
                server
                    .immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let local = lore_storage::local::immutable_store::create(
                    None::<&str>,
                    ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings {
                        implicit_durable_stored: false,
                        ..Default::default()
                    },
                )
                .await?;
                let oodle = lore_base::types::Fragment {
                    flags: fragment.flags
                        | (FragmentFlags::PayloadCompressedOodle2
                            | FragmentFlags::PayloadStoredDurable)
                            .bits(),
                    ..fragment
                };
                local
                    .clone()
                    .put(repository, address, oodle, Some(payload.clone()), false)
                    .await?;

                let session = server.immutable_store.session(repository).await?;
                let options = lore_storage::ReadOptions::default();
                assert!(!options.cache, "the read under test does not ask to cache");
                let (_, content) = lore_storage::read::load_fragment(
                    local.clone(),
                    repository,
                    address,
                    options,
                    Some(session),
                )
                .await?;
                assert_eq!(content, payload);

                let stored = local.clone().get_metadata(repository, address).await?;
                assert_eq!(
                    stored.fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
                    0,
                    "the local entry still claims Oodle"
                );
                let (_, content) = lore_storage::read::load_fragment(
                    local,
                    repository,
                    address,
                    options.no_remote(),
                    None,
                )
                .await?;
                assert_eq!(content, payload);

                Ok(())
            })
            .await
    }

    /// A compressed payload is served as the remote stored it, with the content its check on
    /// receipt expanded. A read placing it, a load and a read into a slice through the store, as a
    /// server whose store is a remote store makes them, take that content, decoding the payload
    /// once each.
    #[tokio::test]
    async fn a_remote_payload_is_served_with_the_content_its_check_expanded() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let content: Vec<u8> = (0..4096).map(|index| (index / 64) as u8).collect();
                let plain = lore_base::types::Fragment {
                    flags: 0,
                    size_payload: content.len() as u32,
                    size_content: content.len() as u64,
                };
                let (fragment, payload) =
                    lore_storage::compress::compress(plain, &content, CompressionMode::Lz4)?;
                let address = lore_base::types::Address {
                    hash: lore_storage::hash::hash_slice(&content),
                    context: random(),
                };
                server
                    .immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;
                lore_storage::test_util::reset_counts();

                let got = server
                    .immutable_store
                    .clone()
                    .get(repository, address)
                    .await?;
                assert_eq!(got.payload.as_ref(), Some(&payload));
                assert_ne!(
                    got.fragment.flags & FragmentFlags::PayloadCompressed.bits(),
                    0,
                    "the payload is served as stored"
                );
                let (content_fragment, checked) = got.content.expect("the checked content");
                assert_eq!(checked.as_ref(), content.as_slice());
                let (_, decodes, hashes) = lore_storage::test_util::counts();
                assert_eq!((decodes, hashes), (1, 1), "checked once on receipt");

                lore_storage::test_util::reset_counts();
                let mut buffer = vec![0u8; content.len()];
                // SAFETY: the buffer outlives the read and nothing else touches it.
                let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
                let (placed, read) = server
                    .immutable_store
                    .clone()
                    .get_into(repository, address, &mut dst)
                    .await?;
                assert!(
                    matches!(read, PayloadRead::IntoBuffer),
                    "the checked content is placed, not the payload returned to expand again"
                );
                assert_eq!(placed, content_fragment);
                assert_eq!(buffer, content);
                assert_eq!(
                    lore_storage::test_util::counts().1,
                    1,
                    "decoded once to place"
                );

                let store: Arc<dyn ImmutableStore> = server.immutable_store.clone();
                lore_storage::test_util::reset_counts();
                let (_, loaded) = lore_storage::read::load_fragment(
                    store.clone(),
                    repository,
                    address,
                    ReadOptions::default().no_remote(),
                    None,
                )
                .await?;
                assert_eq!(loaded.as_ref(), content.as_slice());
                assert_eq!(
                    lore_storage::test_util::counts().1,
                    1,
                    "decoded once to load"
                );

                lore_storage::test_util::reset_counts();
                let mut slice = vec![0u8; content.len()];
                lore_storage::read_into(
                    store,
                    repository,
                    address,
                    None,
                    &mut slice,
                    ReadOptions::default().no_remote(),
                    None,
                )
                .await?;
                assert_eq!(slice, content);
                assert_eq!(
                    lore_storage::test_util::counts().1,
                    1,
                    "decoded once to read into a slice"
                );

                Ok(())
            })
            .await
    }

    /// A payload the backend holds under an address it does not hash to fails the remote store's
    /// check on receipt rather than being served. The in-memory backend's verify refuses the heal
    /// before it hashes, so the one hash counted on the test's thread is the store's.
    #[tokio::test]
    async fn a_remote_payload_not_matching_its_address_is_refused() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (fragment, address, _) = fragment::generate_random();
                let (_, _, other_payload) = fragment::generate_random();
                server
                    .backend
                    .clone()
                    .put(repository, address, fragment, Some(other_payload), false)
                    .await?;
                lore_storage::test_util::reset_counts();

                let err = server
                    .immutable_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect_err("a payload not matching its address is refused");
                assert!(
                    err.is_internal(),
                    "the check on receipt refuses it, got: {err}"
                );
                assert_eq!(
                    lore_storage::test_util::counts().2,
                    1,
                    "the payload was hashed on receipt"
                );

                Ok(())
            })
            .await
    }

    /// A payload the backend stores in a codec this build cannot decode, one no build assigns,
    /// fails the check on receipt as `NotSupported`, and a load, a read into a slice and a read
    /// into a buffer through the store report it so rather than as missing.
    #[tokio::test]
    async fn an_undecodable_remote_payload_is_not_supported() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (plain, address, payload) = fragment::generate_random();
                let unassigned = FragmentFlags::PayloadCompressed
                    & !(FragmentFlags::PayloadCompressedLZ4
                        | FragmentFlags::PayloadCompressedOodle2
                        | FragmentFlags::PayloadCompressedZstd);
                let undecodable = lore_base::types::Fragment {
                    flags: plain.flags | unassigned.bits(),
                    ..plain
                };
                lore_storage::test_util::accepting_unvalidated_puts(server.backend.clone().put(
                    repository,
                    address,
                    undecodable,
                    Some(payload),
                    false,
                ))
                .await?;

                let got = server
                    .immutable_store
                    .clone()
                    .get(repository, address)
                    .await;
                assert!(
                    got.as_ref().is_err_and(|err| err.is_not_supported()),
                    "got {got:?}"
                );
                let store: Arc<dyn ImmutableStore> = server.immutable_store.clone();
                let loaded = lore_storage::read::load_fragment(
                    store.clone(),
                    repository,
                    address,
                    ReadOptions::default().no_remote(),
                    None,
                )
                .await;
                assert!(
                    loaded.as_ref().is_err_and(|err| err.is_not_supported()),
                    "got {loaded:?}"
                );
                let size = plain.size_content as usize;
                let mut slice = vec![0u8; size];
                let sliced = lore_storage::read_into(
                    store.clone(),
                    repository,
                    address,
                    None,
                    &mut slice,
                    ReadOptions::default().no_remote(),
                    None,
                )
                .await;
                assert!(
                    sliced.as_ref().is_err_and(|err| err.is_not_supported()),
                    "got {sliced:?}"
                );
                let mut buffer = vec![0u8; size];
                // SAFETY: the buffer outlives the read and nothing else touches it.
                let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
                let placed = lore_storage::read_into_buffer(
                    store,
                    repository,
                    address,
                    None,
                    &mut dst,
                    ReadOptions::default().no_remote(),
                    None,
                )
                .await;
                assert!(
                    placed.as_ref().is_err_and(|err| err.is_not_supported()),
                    "got {placed:?}"
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_get_not_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (_, address, _) = fragment::generate_random();

                let result = server
                    .immutable_store
                    .clone()
                    .get(repository, address)
                    .await;

                assert!(result.is_err());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_exist_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (fragment, address, payload) = fragment::generate_random();

                server
                    .immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await?;

                let match_result = query_one(
                    &(server.immutable_store.clone() as Arc<dyn ImmutableStore>),
                    repository,
                    address,
                )
                .await?;

                assert_eq!(StoreMatch::MatchFull, match_result.match_made);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_exist_not_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (_, address, _) = fragment::generate_random();

                let match_result = query_one(
                    &(server.immutable_store.clone() as Arc<dyn ImmutableStore>),
                    repository,
                    address,
                )
                .await?;

                assert_eq!(StoreMatch::MatchNone, match_result.match_made);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_exist_batch() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();

                let (frag1, addr1, pay1) = fragment::generate_random();
                let (frag2, addr2, pay2) = fragment::generate_random();
                let (frag3, addr3, pay3) = fragment::generate_random();
                let (_, missing1, _) = fragment::generate_random();
                let (_, missing2, _) = fragment::generate_random();

                server
                    .immutable_store
                    .clone()
                    .put(repository, addr1, frag1, Some(pay1), false)
                    .await?;
                server
                    .immutable_store
                    .clone()
                    .put(repository, addr2, frag2, Some(pay2), false)
                    .await?;
                server
                    .immutable_store
                    .clone()
                    .put(repository, addr3, frag3, Some(pay3), false)
                    .await?;

                let addresses = [addr1, addr2, missing1, addr3, missing2];
                let mut results = [StoreMatchResult::default(); 5];
                server
                    .immutable_store
                    .clone()
                    .query(repository, &addresses, &mut results)
                    .await?;

                assert_eq!(StoreMatch::MatchFull, results[0].match_made);
                assert_eq!(StoreMatch::MatchFull, results[1].match_made);
                assert_eq!(StoreMatch::MatchNone, results[2].match_made);
                assert_eq!(StoreMatch::MatchFull, results[3].match_made);
                assert_eq!(StoreMatch::MatchNone, results[4].match_made);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_query_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (fragment, address, payload) = fragment::generate_random();

                server
                    .immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await?;

                let result = server
                    .immutable_store
                    .clone()
                    .get_metadata(repository, address)
                    .await?;

                assert_eq!(StoreMatch::MatchFull, result.match_made);
                assert_eq!(fragment.size_payload, result.fragment.size_payload);
                assert_eq!(fragment.size_content, result.fragment.size_content);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_query_not_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let (_, address, _) = fragment::generate_random();

                let result = server
                    .immutable_store
                    .clone()
                    .get_metadata(repository, address)
                    .await?;

                assert_eq!(StoreMatch::MatchNone, result.match_made);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_immutable_copy() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repo_a = random::<RepositoryId>();
                let repo_b = random::<RepositoryId>();
                let (fragment, address, payload) = fragment::generate_random();

                server
                    .immutable_store
                    .clone()
                    .put(repo_a, address, fragment, Some(payload.clone()), false)
                    .await?;

                server
                    .immutable_store
                    .clone()
                    .copy(
                        repo_a,
                        address,
                        repo_b,
                        address.context,
                        CopyBehavior {
                            durable: false,
                            do_not_replicate: false,
                        },
                    )
                    .await?;

                let (got_fragment, got_payload) = server
                    .immutable_store
                    .clone()
                    .get(repo_b, address)
                    .await
                    .and_then(StoreGetData::into_payload)?;

                assert_eq!(payload, got_payload);
                assert_eq!(fragment.size_payload, got_fragment.size_payload);

                Ok(())
            })
            .await
    }

    // ── Mutable Store Tests ──

    #[tokio::test]
    async fn test_mutable_store_and_load() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();
                let value = random::<Hash>();

                server
                    .mutable_store
                    .clone()
                    .store(repository, key, value, KeyType::Untyped)
                    .await?;

                let loaded = server
                    .mutable_store
                    .clone()
                    .load(repository, key, KeyType::Untyped)
                    .await?;

                assert_eq!(value, loaded);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_load_not_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();

                let result = server
                    .mutable_store
                    .clone()
                    .load(repository, key, KeyType::Untyped)
                    .await;

                assert!(result.as_ref().is_err_and(|e| e.is_address_not_found()));

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_store_overwrite() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();
                let value_a = random::<Hash>();
                let value_b = random::<Hash>();

                server
                    .mutable_store
                    .clone()
                    .store(repository, key, value_a, KeyType::Untyped)
                    .await?;

                server
                    .mutable_store
                    .clone()
                    .store(repository, key, value_b, KeyType::Untyped)
                    .await?;

                let loaded = server
                    .mutable_store
                    .clone()
                    .load(repository, key, KeyType::Untyped)
                    .await?;

                assert_eq!(value_b, loaded);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_compare_and_swap() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();
                let expected = random::<Hash>();
                let value = random::<Hash>();
                let different = random::<Hash>();

                server
                    .mutable_store
                    .clone()
                    .store(repository, key, expected, KeyType::Untyped)
                    .await?;

                // CAS with wrong expected should not swap, returns current value
                assert_eq!(
                    expected,
                    server
                        .mutable_store
                        .clone()
                        .compare_and_swap(repository, key, different, value, KeyType::Untyped)
                        .await?
                );

                // Value should still be expected
                assert_eq!(
                    expected,
                    server
                        .mutable_store
                        .clone()
                        .load(repository, key, KeyType::Untyped)
                        .await?
                );

                // CAS with correct expected should swap, returns previous value
                assert_eq!(
                    expected,
                    server
                        .mutable_store
                        .clone()
                        .compare_and_swap(repository, key, expected, value, KeyType::Untyped)
                        .await?
                );

                // Value should now be the new value
                assert_eq!(
                    value,
                    server
                        .mutable_store
                        .clone()
                        .load(repository, key, KeyType::Untyped)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_compare_and_swap_not_found() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();
                let expected = random::<Hash>();
                let value = random::<Hash>();

                // CAS on non-existent key with non-default expected returns Hash::default()
                assert_eq!(
                    Hash::default(),
                    server
                        .mutable_store
                        .clone()
                        .compare_and_swap(repository, key, expected, value, KeyType::Untyped)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_compare_and_swap_create_from_empty() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();
                let value = random::<Hash>();

                // CAS on non-existent key with default expected should create the entry
                assert_eq!(
                    Hash::default(),
                    server
                        .mutable_store
                        .clone()
                        .compare_and_swap(repository, key, Hash::default(), value, KeyType::Untyped)
                        .await?
                );

                // The value should now be stored
                assert_eq!(
                    value,
                    server
                        .mutable_store
                        .clone()
                        .load(repository, key, KeyType::Untyped)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_store_zero_deletes() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repository = random::<RepositoryId>();
                let key = random::<Hash>();
                let value = random::<Hash>();

                server
                    .mutable_store
                    .clone()
                    .store(repository, key, value, KeyType::Untyped)
                    .await?;

                assert_eq!(
                    value,
                    server
                        .mutable_store
                        .clone()
                        .load(repository, key, KeyType::Untyped)
                        .await?
                );

                // Storing zero hash should delete the entry
                server
                    .mutable_store
                    .clone()
                    .store(repository, key, Hash::default(), KeyType::Untyped)
                    .await?;

                let result = server
                    .mutable_store
                    .clone()
                    .load(repository, key, KeyType::Untyped)
                    .await;
                assert!(result.as_ref().is_err_and(|e| e.is_address_not_found()));

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_mutable_repository_isolation() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let repo_a = random::<RepositoryId>();
                let repo_b = random::<RepositoryId>();
                let key = random::<Hash>();
                let value = random::<Hash>();

                server
                    .mutable_store
                    .clone()
                    .store(repo_a, key, value, KeyType::Untyped)
                    .await?;

                assert_eq!(
                    value,
                    server
                        .mutable_store
                        .clone()
                        .load(repo_a, key, KeyType::Untyped)
                        .await?
                );

                // Loading from a different repository should not find the key
                let result = server
                    .mutable_store
                    .clone()
                    .load(repo_b, key, KeyType::Untyped)
                    .await;
                assert!(result.as_ref().is_err_and(|e| e.is_address_not_found()));

                Ok(())
            })
            .await
    }
}
