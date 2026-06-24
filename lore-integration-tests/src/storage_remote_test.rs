// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the content-addressed storage API's remote-backed paths.
//!
//! Spin up a real gRPC server backed by a `LocalImmutableStore` in process; open a storage
//! handle with `remote_config` pointing at it; exercise the remote-touching ops. Gated under
//! the `integration_tests` feature so default `cargo test` (which does not start servers)
//! stays fast.

#[cfg(all(test, feature = "integration_tests"))]
mod storage_remote_tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use lore::storage::close;
    use lore::storage::open;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore::storage::open::LoreStorageRemoteConfig;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_server::grpc::server::FeatureSettings;
    use lore_server::grpc::server::GrpcServerBuilder;
    use lore_server::hooks::HookDispatcher;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;

    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    struct TestServer {
        url: String,
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    async fn start_test_server() -> TestServer {
        let backend_immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                allow_partial_fragment: false,
                protect_local_fragment: false,
                implicit_durable_stored: true,
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
        let backend_for_test = backend_immutable.clone();
        let backend_mutable_for_test: Arc<dyn lore_storage::MutableStore> = backend_mutable.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        drop(listener);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let signal = async {
            shutdown_rx.await.ok();
        };

        let notification_sender: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());
        let hook_dispatcher = Arc::new(HookDispatcher::empty());

        // Background server task in a test; LORE_CONTEXT propagation is unnecessary here.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            GrpcServerBuilder::new()
                .with_environment(EnvironmentConfig::default())
                .with_feature(FeatureSettings::default())
                .with_immutable_store(backend_immutable.clone(), backend_immutable)
                .with_mutable_store(backend_mutable)
                .with_lock_store(None)
                .with_notification(notification_sender, None)
                .with_hook_dispatcher(hook_dispatcher)
                .with_tls_config(None, None, None)
                .unwrap()
                .with_admin_endpoints(HashMap::new(), vec![])
                .with_http2_config(
                    None,
                    None,
                    Duration::from_secs(30),
                    None,
                    Default::default(),
                )
                .with_jwt_verifier(None)
                .unwrap()
                .serve(addr, signal)
                .await
                .unwrap();
        });

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
        }

        TestServer {
            url: format!("grpc://127.0.0.1:{}", addr.port()),
            backend_immutable: backend_for_test,
            backend_mutable: backend_mutable_for_test,
            _shutdown: shutdown_tx,
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Captured {
        Opened { handle_id: u64 },
        Error,
        Complete(i32),
        Other,
    }

    fn make_sink() -> (Arc<Mutex<Vec<Captured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageOpened(data) => Captured::Opened {
                    handle_id: data.handle_id,
                },
                LoreEvent::Error(_) => Captured::Error,
                LoreEvent::Complete(data) => Captured::Complete(data.status),
                _ => Captured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    fn take_opened(events: &[Captured]) -> Option<u64> {
        events.iter().find_map(|e| match e {
            Captured::Opened { handle_id } => Some(*handle_id),
            _ => None,
        })
    }

    async fn open_remote_handle(server: &TestServer) -> u64 {
        let (sink, callback) = make_sink();
        let status = open::open(
            LoreGlobalArgs::default(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                remote_config: LoreStorageRemoteConfig {
                    remote_url: LoreString::from(server.url.as_str()),
                },
                has_remote_config: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "open with remote_config must succeed");
        let events = sink.lock().unwrap().clone();
        take_opened(&events).expect("STORAGE_OPENED must fire on remote-configured open")
    }

    async fn close_handle(handle_id: u64) {
        let close_status = close::close(
            LoreGlobalArgs::default(),
            lore::storage::close::LoreStorageCloseArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
            },
            None,
        )
        .await;
        assert_eq!(close_status, 0, "close must succeed");
    }

    #[tokio::test]
    async fn open_with_remote_config_succeeds_against_real_server() -> TestResult {
        let execution = setup_execution("storage-remote-open".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;

                let (sink, callback) = make_sink();
                let status = open::open(
                    LoreGlobalArgs::default(),
                    LoreStorageOpenArgs {
                        repository_path: LoreString::default(),
                        in_memory: 1,
                        remote_config: LoreStorageRemoteConfig {
                            remote_url: LoreString::from(server.url.as_str()),
                        },
                        has_remote_config: 1,
                        ..Default::default()
                    },
                    callback,
                )
                .await;

                assert_eq!(status, 0, "open with remote_config must succeed");
                let events = sink.lock().unwrap().clone();
                let handle_id = take_opened(&events)
                    .expect("STORAGE_OPENED must fire on remote-configured open");
                assert!(handle_id != 0, "handle_id must be non-zero");

                close_handle(handle_id).await;

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn put_with_remote_write_uploads_payload_to_server() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;

                let payload = b"phase-d remote upload payload".to_vec();
                let partition = Partition::from([0xa7u8; 16]);

                let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.id,
                            data.address,
                            data.error_code,
                        ));
                    }
                }));

                let item = LoreStoragePutItem {
                    id: 1,
                    partition,
                    context: Context::default(),
                    data: LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 1,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                let status = put::put(
                    LoreGlobalArgs::default(),
                    LoreStoragePutArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0, "put with remote_write=1 must succeed");

                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1, "exactly one PUT_ITEM_COMPLETE expected");
                let (id, address, code) = events[0];
                assert_eq!(id, 1);
                assert_eq!(code, LoreErrorCode::None);
                assert_ne!(address.hash, lore_base::types::Hash::default());

                let server_match = server
                    .backend_immutable
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("backend exist call");
                assert_eq!(
                    server_match,
                    StoreMatch::MatchFull,
                    "remote backend must hold the address after remote_write=1 put",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn put_without_remote_write_does_not_upload() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put-localonly".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;

                let payload = b"local-only put (remote_write=0)".to_vec();
                let partition = Partition::from([0xa8u8; 16]);

                let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.id,
                            data.address,
                            data.error_code,
                        ));
                    }
                }));

                let item = LoreStoragePutItem {
                    id: 42,
                    partition,
                    context: Context::default(),
                    data: LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 0,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                let status = put::put(
                    LoreGlobalArgs::default(),
                    LoreStoragePutArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);

                let events = captured.lock().unwrap().clone();
                let (_, address, code) = events[0];
                assert_eq!(code, LoreErrorCode::None);

                let server_match = server
                    .backend_immutable
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("backend exist call");
                assert_eq!(
                    server_match,
                    StoreMatch::MatchNone,
                    "remote backend must NOT hold the address when remote_write=0",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn get_falls_back_to_remote_on_local_miss() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-get".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;

                // Seed the server-side backend directly so the handle's local store starts as a
                // miss for this address. The hash and fragment match the bytes — the server
                // accepts this as a valid put.
                let payload_bytes = b"phase-d remote get on miss".to_vec();
                let payload = Bytes::from(payload_bytes.clone());
                let partition = Partition::from([0xb1u8; 16]);
                let hash = lore_storage::hash_slice(payload.as_ref());
                let address = Address {
                    hash,
                    context: Context::default(),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                server
                    .backend_immutable
                    .clone()
                    .put(partition, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("seed server with payload");

                let handle_id = open_remote_handle(&server).await;

                // The handle's local in-memory store is empty for this address — get must reach
                // the configured remote, return the bytes, and cache the entry locally.
                let received: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                let received_for_cb = received.clone();
                let outcomes: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let outcomes_for_cb = outcomes.clone();
                let callback: LoreEventCallback =
                    Some(Box::new(move |event: &LoreEvent| match event {
                        LoreEvent::StorageGetData(data) => {
                            let slice = unsafe {
                                std::slice::from_raw_parts(
                                    data.bytes.ptr.cast::<u8>(),
                                    data.bytes.len,
                                )
                            };
                            received_for_cb.lock().unwrap().extend_from_slice(slice);
                        }
                        LoreEvent::StorageGetItemComplete(data) => {
                            outcomes_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                        _ => {}
                    }));

                let item = LoreStorageGetItem {
                    id: 7,
                    partition,
                    address,
                    streaming: 0,
                    local_cache: 0,
                };
                let status = get::get(
                    LoreGlobalArgs::default(),
                    LoreStorageGetArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0, "get must succeed against remote-only address");

                let outcomes = outcomes.lock().unwrap().clone();
                assert_eq!(outcomes.len(), 1);
                assert_eq!(outcomes[0], (7, LoreErrorCode::None));

                let received = received.lock().unwrap().clone();
                assert_eq!(received, payload_bytes, "fetched bytes must match remote");

                // The remote-fetched payload was not flagged with `PayloadLocalCachePriority`,
                // so `get` does NOT write the bytes back to the local store. Verify by
                // probing the handle's local immutable store directly — `MatchFull` must miss.
                let local =
                    lore::storage::handle::immutable_for_test(lore::storage::handle::LoreStore {
                        handle_id,
                    })
                    .expect("handle still registered");
                let local_match = local
                    .clone()
                    .exist(
                        partition,
                        address,
                        lore_storage::store_types::StoreMatch::MatchFull,
                    )
                    .await
                    .expect("local exist call");
                assert_eq!(
                    local_match,
                    lore_storage::store_types::StoreMatch::MatchNone,
                    "unflagged remote-fetched payload must NOT be cached locally",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// `get` does not blanket-cache remote-fetched payloads, but a producer-side write hint
    /// (`PayloadLocalCachePriority` set on the seed fragment) opts the payload into local
    /// caching via `load_fragment`'s `should_store` gate. Verify the gate fires for that
    /// case so producers retain the ability to mark "this should always be cached".
    #[tokio::test]
    async fn get_caches_locally_when_payload_has_local_cache_priority_flag() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_base::types::fragment_flags::FragmentFlags;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-cache-priority".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;

                // Seed the server with a fragment whose flags include the local-cache-priority
                // hint. The handle's local store starts empty for this address; after the
                // get, the priority flag must trigger a local cache populate.
                let payload_bytes = b"priority-flagged payload".to_vec();
                let payload = Bytes::from(payload_bytes.clone());
                let partition = Partition::from([0xc7u8; 16]);
                let hash = lore_storage::hash_slice(payload.as_ref());
                let address = Address {
                    hash,
                    context: Context::default(),
                };
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadLocalCachePriority.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                server
                    .backend_immutable
                    .clone()
                    .put(partition, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("seed server with priority-flagged payload");

                let handle_id = open_remote_handle(&server).await;

                let outcomes: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let outcomes_for_cb = outcomes.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetItemComplete(data) = event {
                        outcomes_for_cb
                            .lock()
                            .unwrap()
                            .push((data.id, data.error_code));
                    }
                }));
                let status = get::get(
                    LoreGlobalArgs::default(),
                    LoreStorageGetArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageGetItem {
                            id: 41,
                            partition,
                            address,
                            streaming: 0,
                            local_cache: 0,
                        }]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                assert_eq!(outcomes.lock().unwrap()[0].1, LoreErrorCode::None);

                let local =
                    lore::storage::handle::immutable_for_test(lore::storage::handle::LoreStore {
                        handle_id,
                    })
                    .expect("handle still registered");
                let local_match = local
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("local exist call");
                assert_eq!(
                    local_match,
                    StoreMatch::MatchFull,
                    "priority-flagged remote-fetched payload must be cached locally",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// `local_cache=1` on a get item forces `with_cache()` for that fetch even when the
    /// fragment is not flagged with `PayloadLocalCachePriority`. The companion
    /// `get_falls_back_to_remote_on_local_miss` test asserts the opposite (no flag, no
    /// per-item opt-in → no cache); this one proves the per-item opt-in works.
    #[tokio::test]
    async fn get_with_local_cache_flag_caches_remote_fetched_payload_locally() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-get-localcache".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let payload_bytes = b"per-item local_cache opt-in".to_vec();
                let payload = Bytes::from(payload_bytes.clone());
                let partition = Partition::from([0xc8u8; 16]);
                let address = Address {
                    hash: lore_storage::hash_slice(payload.as_ref()),
                    context: Context::default(),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                server
                    .backend_immutable
                    .clone()
                    .put(partition, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("seed server");
                let handle_id = open_remote_handle(&server).await;

                let outcomes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                let outcomes_for_cb = outcomes.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetItemComplete(data) = event {
                        outcomes_for_cb.lock().unwrap().push(data.error_code);
                    }
                }));
                let status = get::get(
                    LoreGlobalArgs::default(),
                    LoreStorageGetArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageGetItem {
                            id: 51,
                            partition,
                            address,
                            streaming: 0,
                            local_cache: 1,
                        }]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                assert_eq!(outcomes.lock().unwrap()[0], LoreErrorCode::None);

                let local =
                    lore::storage::handle::immutable_for_test(lore::storage::handle::LoreStore {
                        handle_id,
                    })
                    .expect("handle still registered");
                let local_match = local
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("local exist");
                assert_eq!(
                    local_match,
                    StoreMatch::MatchFull,
                    "local_cache=1 must populate the local store after remote fetch",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// `local_cache=1` on a put item tags the resulting fragment with
    /// `PayloadLocalCachePriority`. A subsequent local query of the address shows the flag
    /// is preserved so future remote reads of this content cache regardless of the reader's
    /// caching choice.
    #[tokio::test]
    async fn put_with_local_cache_flag_tags_fragment_with_priority() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put-localcache".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;

                let payload = b"put with local_cache priority".to_vec();
                let partition = Partition::from([0xc9u8; 16]);
                let context = Context::from([0xa9u8; 16]);

                let captured: Arc<Mutex<Vec<(Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(data) = event {
                        captured_for_cb
                            .lock()
                            .unwrap()
                            .push((data.address, data.error_code));
                    }
                }));
                let item = LoreStoragePutItem {
                    id: 61,
                    partition,
                    context,
                    data: LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 0,
                    local_cache: 1,
                    fixed_size_chunk: 0,
                };
                let status = put::put(
                    LoreGlobalArgs::default(),
                    LoreStoragePutArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                let (address, code) = captured.lock().unwrap()[0];
                assert_eq!(code, LoreErrorCode::None);
                drop(payload);

                // Read the resulting fragment back from the local store and assert the
                // priority flag rode through `write_content`.
                let local =
                    lore::storage::handle::immutable_for_test(lore::storage::handle::LoreStore {
                        handle_id,
                    })
                    .expect("handle still registered");
                let (fragment, _bytes) = local
                    .clone()
                    .get(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("local fragment fetch");
                assert!(
                    fragment.flags
                        & lore_base::types::fragment_flags::FragmentFlags::PayloadLocalCachePriority
                            .bits() != 0,
                    "local_cache=1 must set PayloadLocalCachePriority on the fragment; \
                     got flags={:#x}",
                    fragment.flags,
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    async fn put_local_via_handle(
        handle_id: u64,
        partition: lore_base::types::Partition,
        bytes: &[u8],
    ) -> lore_base::types::Address {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Context;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.address, data.error_code));
            }
        }));
        let item = LoreStoragePutItem {
            id: 99,
            partition,
            context: Context::default(),
            data: LoreBytes {
                ptr: bytes.as_ptr().cast(),
                len: bytes.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let status = put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = captured.lock().unwrap().clone();
        assert_eq!(events[0].2, LoreErrorCode::None);
        events[0].1
    }

    async fn copy_one_item(
        handle_id: u64,
        source_partition: lore_base::types::Partition,
        source_address: lore_base::types::Address,
        target_partition: lore_base::types::Partition,
    ) -> (
        i32,
        Vec<(
            u64,
            lore_base::types::Address,
            lore_revision::event::LoreErrorCode,
        )>,
    ) {
        use lore::storage::copy;
        use lore::storage::copy::LoreStorageCopyArgs;
        use lore::storage::copy::LoreStorageCopyItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageCopyItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push((
                    data.id,
                    data.source_address,
                    data.error_code,
                ));
            }
        }));
        let status = copy::copy(
            LoreGlobalArgs::default(),
            LoreStorageCopyArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageCopyItem {
                    id: 11,
                    source_partition,
                    target_partition,
                    source_address,
                    target_context: source_address.context,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Put source via the handle with `remote_write=1`, so the bytes land both locally and on
    /// the server. Returns the resulting address.
    async fn put_local_and_remote_via_handle(
        handle_id: u64,
        partition: lore_base::types::Partition,
        bytes: &[u8],
    ) -> lore_base::types::Address {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Context;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.address, data.error_code));
            }
        }));
        let item = LoreStoragePutItem {
            id: 100,
            partition,
            context: Context::default(),
            data: LoreBytes {
                ptr: bytes.as_ptr().cast(),
                len: bytes.len(),
            },
            remote_write: 1,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let status = put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = captured.lock().unwrap().clone();
        assert_eq!(events[0].2, LoreErrorCode::None);
        events[0].1
    }

    #[tokio::test]
    async fn copy_tier1_server_side_when_source_on_both() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-copy-tier1".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let source_partition = Partition::from([0xc1u8; 16]);
                let target_partition = Partition::from([0xc2u8; 16]);

                let handle_id = open_remote_handle(&server).await;
                // Source on both: handle puts with remote_write=1, landing the bytes locally
                // and on the server in source_partition. This is the realistic shape — local
                // mirror after server-side copy can adopt the source's local payload via the
                // existing `ImmutableStore::copy(.., durable=true)` primitive.
                let payload_bytes = b"copy tier-1 payload (both local and server)".to_vec();
                let address = put_local_and_remote_via_handle(
                    handle_id,
                    source_partition,
                    payload_bytes.as_slice(),
                )
                .await;

                let (status, events) =
                    copy_one_item(handle_id, source_partition, address, target_partition).await;
                assert_eq!(status, 0);
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].2, LoreErrorCode::None);
                assert_eq!(events[0].1, address);

                let on_server = server
                    .backend_immutable
                    .clone()
                    .exist(target_partition, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();
                assert_eq!(
                    on_server,
                    StoreMatch::MatchFull,
                    "server must hold target entry after server-side copy",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn copy_tier2_upload_fallback_when_local_source_only() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-copy-tier2".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let source_partition = Partition::from([0xd1u8; 16]);
                let target_partition = Partition::from([0xd2u8; 16]);

                let handle_id = open_remote_handle(&server).await;
                // Source exists ONLY locally on the handle (remote_write=0). The server has
                // nothing in source_partition, so a server-side copy is impossible — the copy
                // op must fall back to upload + local copy with durable=true.
                let payload_bytes = b"copy tier-2 upload-fallback payload".to_vec();
                let address =
                    put_local_via_handle(handle_id, source_partition, payload_bytes.as_slice())
                        .await;

                let server_pre = server
                    .backend_immutable
                    .clone()
                    .exist(source_partition, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();
                assert_eq!(
                    server_pre,
                    StoreMatch::MatchNone,
                    "precondition: server must NOT have source",
                );

                let (status, events) =
                    copy_one_item(handle_id, source_partition, address, target_partition).await;
                assert_eq!(status, 0);
                assert_eq!(events[0].2, LoreErrorCode::None);
                assert_eq!(events[0].1, address);

                // After tier-2 fallback the server now holds the target tuple (uploaded into
                // the destination's partition).
                let on_server = server
                    .backend_immutable
                    .clone()
                    .exist(target_partition, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();
                assert_eq!(
                    on_server,
                    StoreMatch::MatchFull,
                    "server must hold target after upload fallback",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    async fn obliterate_one_item(
        handle_id: u64,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
    ) -> (
        i32,
        Vec<(
            u64,
            lore_base::types::Address,
            u8,
            u8,
            u8,
            u8,
            lore_revision::event::LoreErrorCode,
        )>,
    ) {
        use lore::storage::obliterate;
        use lore::storage::obliterate::LoreStorageObliterateArgs;
        use lore::storage::obliterate::LoreStorageObliterateItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        #[allow(clippy::type_complexity)]
        let captured: Arc<
            Mutex<
                Vec<(
                    u64,
                    lore_base::types::Address,
                    u8,
                    u8,
                    u8,
                    u8,
                    LoreErrorCode,
                )>,
            >,
        > = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageObliterateItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push((
                    data.id,
                    data.address,
                    data.local_success,
                    data.remote_success,
                    data.local_skipped,
                    data.remote_skipped,
                    data.error_code,
                ));
            }
        }));
        let status = obliterate::obliterate(
            LoreGlobalArgs::default(),
            LoreStorageObliterateArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageObliterateItem {
                    id: 31,
                    partition,
                    address,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Verify obliterate runs both sides and reports each side's outcome separately. The test
    /// server is configured with no JWT verifier, so the admin obliterate path returns
    /// `Ok` — `remote_success=1` and `error_code=None` are the expected
    /// outcomes. `local_success=1` confirms the local side ran independently and succeeded.
    #[tokio::test]
    async fn obliterate_runs_local_and_remote_in_parallel_with_independent_outcomes() -> TestResult
    {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-obliterate".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xb5u8; 16]);

                let handle_id = open_remote_handle(&server).await;
                let payload_bytes = b"obliterate me everywhere".to_vec();
                let address =
                    put_local_and_remote_via_handle(handle_id, partition, payload_bytes.as_slice())
                        .await;

                let (_status, events) = obliterate_one_item(handle_id, partition, address).await;
                assert_eq!(events.len(), 1);
                let (id, _addr, local_success, remote_success, _ls, _rs, error_code) = events[0];
                assert_eq!(id, 31);
                assert_eq!(local_success, 1, "local obliterate must succeed");
                assert_eq!(remote_success, 1, "without JWT the admin path is allowed",);
                assert_eq!(
                    error_code,
                    LoreErrorCode::None,
                    "Remote obliterate should have succeeded",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// An absent local address still reports `local_success=1` (idempotent obliterate).
    /// The remote side fails for the same JWT reason, which is independent of presence.
    #[tokio::test]
    async fn obliterate_absent_address_is_idempotent_on_local_side() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let execution = setup_execution("storage-remote-obliterate-absent".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xb6u8; 16]);
                let handle_id = open_remote_handle(&server).await;
                let address = Address {
                    hash: Hash::from([0xddu8; 32]),
                    context: Context::default(),
                };
                let (_status, events) = obliterate_one_item(handle_id, partition, address).await;
                let (_, _, local_success, _, _, _, _) = events[0];
                assert_eq!(local_success, 1, "absent address still succeeds locally");

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn query_falls_back_to_remote_when_local_misses() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-query-multiplex".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xc7u8; 16]);

                // Seed N distinct addresses on the server's backend in the same partition.
                // The handle's local store starts empty, so every query item misses locally
                // and falls through to the multiplexed remote path.
                let n = 10usize;
                let mut payloads: Vec<Vec<u8>> = (0..n)
                    .map(|i| format!("multiplexed-query-payload-{i}").into_bytes())
                    .collect();
                let mut addresses: Vec<Address> = Vec::with_capacity(n);
                for payload in &payloads {
                    let bytes = Bytes::from(payload.clone());
                    let hash = lore_storage::hash_slice(bytes.as_ref());
                    let address = Address {
                        hash,
                        context: Context::default(),
                    };
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: bytes.len() as u32,
                        size_content: bytes.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(partition, address, fragment, Some(bytes), false)
                        .await
                        .expect("seed query item");
                    addresses.push(address);
                }
                payloads.clear();

                let handle_id = open_remote_handle(&server).await;

                let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetMetadataItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.id,
                            data.address,
                            data.error_code,
                        ));
                    }
                }));

                use lore::storage::get_metadata;
                use lore::storage::get_metadata::LoreStorageGetMetadataArgs;
                use lore::storage::get_metadata::LoreStorageGetMetadataItem;
                let items: Vec<LoreStorageGetMetadataItem> = addresses
                    .iter()
                    .enumerate()
                    .map(|(i, addr)| LoreStorageGetMetadataItem {
                        id: i as u64,
                        partition,
                        address: *addr,
                    })
                    .collect();
                let status = get_metadata::get_metadata(
                    LoreGlobalArgs::default(),
                    LoreStorageGetMetadataArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(items),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0, "all items must succeed via remote get_metadata");

                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), n);
                // Remote items are spawned into a JoinSet and complete in non-deterministic
                // order — events from different items interleave freely. Sort by id to put
                // assertions on a deterministic footing without imposing an order contract
                // the API doesn't guarantee.
                let mut events = events;
                events.sort_by_key(|(id, _, _)| *id);
                for (i, (id, addr, code)) in events.iter().enumerate() {
                    assert_eq!(*id, i as u64);
                    assert_eq!(*addr, addresses[i]);
                    assert_eq!(*code, LoreErrorCode::None);
                }

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    async fn upload_one_item(
        handle_id: u64,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
    ) -> (
        i32,
        Vec<(
            u64,
            lore_base::types::Address,
            u8,
            lore_revision::event::LoreErrorCode,
        )>,
    ) {
        use lore::storage::upload;
        use lore::storage::upload::LoreStorageUploadArgs;
        use lore::storage::upload::LoreStorageUploadItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        #[allow(clippy::type_complexity)]
        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, u8, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageUploadItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push((
                    data.id,
                    data.address,
                    data.already_durable,
                    data.error_code,
                ));
            }
        }));
        let status = upload::upload(
            LoreGlobalArgs::default(),
            LoreStorageUploadArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageUploadItem {
                    id: 51,
                    partition,
                    address,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn upload_local_only_payload_pushes_to_remote_and_marks_durable() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-upload".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xa5u8; 16]);

                let handle_id = open_remote_handle(&server).await;
                // Stage the item locally only — server doesn't have it. After upload, server
                // must have it AND the local fragment should be marked durable (re-uploading
                // the same address must short-circuit as already_durable=1).
                let payload_bytes = b"upload deferred payload".to_vec();
                let address =
                    put_local_via_handle(handle_id, partition, payload_bytes.as_slice()).await;

                let (status, events) = upload_one_item(handle_id, partition, address).await;
                assert_eq!(status, 0, "upload must succeed");
                assert_eq!(events.len(), 1);
                let (_, _, already_durable, error_code) = events[0];
                assert_eq!(error_code, LoreErrorCode::None);
                assert_eq!(already_durable, 0, "first upload was not yet durable");

                let on_server = server
                    .backend_immutable
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();
                assert_eq!(
                    on_server,
                    StoreMatch::MatchFull,
                    "server must hold the address after a successful upload",
                );

                // Re-upload the same address — should short-circuit as already_durable=1.
                let (status2, events2) = upload_one_item(handle_id, partition, address).await;
                assert_eq!(status2, 0);
                let (_, _, already_durable2, error_code2) = events2[0];
                assert_eq!(error_code2, LoreErrorCode::None);
                assert_eq!(
                    already_durable2, 1,
                    "second upload must short-circuit as already_durable=1",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn upload_unknown_address_returns_address_not_found() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-upload-unknown".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xa6u8; 16]);
                let handle_id = open_remote_handle(&server).await;
                let address = Address {
                    hash: Hash::from([0xfau8; 32]),
                    context: Context::default(),
                };
                let (_, events) = upload_one_item(handle_id, partition, address).await;
                let (_, _, already_durable, error_code) = events[0];
                assert_eq!(error_code, LoreErrorCode::AddressNotFound);
                assert_eq!(already_durable, 0);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn upload_zero_hash_short_circuits_as_already_durable() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-upload-zero".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xa7u8; 16]);
                let handle_id = open_remote_handle(&server).await;
                let address = Address {
                    hash: Hash::default(),
                    context: Context::default(),
                };
                let (status, events) = upload_one_item(handle_id, partition, address).await;
                assert_eq!(status, 0);
                let (_, _, already_durable, error_code) = events[0];
                assert_eq!(error_code, LoreErrorCode::None);
                assert_eq!(already_durable, 1);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn upload_handle_without_remote_fails_pre_dispatch() -> TestResult {
        use lore::storage::upload;
        use lore::storage::upload::LoreStorageUploadArgs;
        use lore::storage::upload::LoreStorageUploadItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-upload-no-remote".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (sink, callback) = make_sink();
                let status = open::open(
                    LoreGlobalArgs::default(),
                    LoreStorageOpenArgs {
                        repository_path: LoreString::default(),
                        in_memory: 1,
                        ..Default::default()
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                let events = sink.lock().unwrap().clone();
                let handle_id = take_opened(&events).expect("opened");

                let item_events: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
                let item_events_for_cb = item_events.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if matches!(event, LoreEvent::StorageUploadItemComplete(_)) {
                        *item_events_for_cb.lock().unwrap() += 1;
                    }
                }));
                let status = upload::upload(
                    LoreGlobalArgs::default(),
                    LoreStorageUploadArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageUploadItem {
                            id: 1,
                            partition: Partition::from([0xa8u8; 16]),
                            address: Address {
                                hash: Hash::from([0xb0u8; 32]),
                                context: Context::default(),
                            },
                        }]),
                    },
                    callback,
                )
                .await;
                assert_ne!(
                    status, 0,
                    "upload without remote_config must fail pre-dispatch"
                );
                assert_eq!(
                    *item_events.lock().unwrap(),
                    0,
                    "no per-item events must fire on pre-dispatch refusal",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Copy idempotency: re-copying onto an already-populated target tuple succeeds with the
    /// same `error_code = None` and produces no observable change. Exercises the remote path
    /// (idempotent on both server-side `session.copy` and the local mirror).
    #[tokio::test]
    async fn copy_idempotent_when_target_already_present() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-copy-idempotent".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let source_partition = Partition::from([0xa1u8; 16]);
                let target_partition = Partition::from([0xa2u8; 16]);

                let handle_id = open_remote_handle(&server).await;
                let payload_bytes = b"copy idempotency payload".to_vec();
                let address = put_local_and_remote_via_handle(
                    handle_id,
                    source_partition,
                    payload_bytes.as_slice(),
                )
                .await;

                let (status1, events1) =
                    copy_one_item(handle_id, source_partition, address, target_partition).await;
                assert_eq!(status1, 0);
                assert_eq!(events1[0].2, LoreErrorCode::None);
                let target_after_first = server
                    .backend_immutable
                    .clone()
                    .exist(target_partition, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();
                assert_eq!(target_after_first, StoreMatch::MatchFull);

                // Second invocation with identical arguments: must return None and leave the
                // target tuple in the same state.
                let (status2, events2) =
                    copy_one_item(handle_id, source_partition, address, target_partition).await;
                assert_eq!(status2, 0);
                assert_eq!(events2[0].2, LoreErrorCode::None);
                assert_eq!(events2[0].1, address);
                let target_after_second = server
                    .backend_immutable
                    .clone()
                    .exist(target_partition, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();
                assert_eq!(
                    target_after_second, target_after_first,
                    "second copy must produce no observable change",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn copy_tier3_no_local_no_server_returns_address_not_found() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-copy-tier3".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let source_partition = Partition::from([0xe1u8; 16]);
                let target_partition = Partition::from([0xe2u8; 16]);

                let handle_id = open_remote_handle(&server).await;

                // No source anywhere — neither in handle's local store nor on the server.
                let address = Address {
                    hash: Hash::from([0xfeu8; 32]),
                    context: Context::default(),
                };
                let (_status, events) =
                    copy_one_item(handle_id, source_partition, address, target_partition).await;
                assert_eq!(
                    events[0].2,
                    LoreErrorCode::AddressNotFound,
                    "tier-3: no local payload + server-side copy fails ⇒ ADDRESS_NOT_FOUND",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// `put_file` with `remote_write=1` against a remote-configured handle must upload to the
    /// server. Exercises the path that previously hardcoded `None` for the `remote_session` and
    /// silently dropped the upload.
    #[tokio::test]
    async fn put_file_with_remote_write_uploads_file_to_server() -> TestResult {
        use lore::storage::put_file;
        use lore::storage::put_file::LoreStoragePutFileArgs;
        use lore::storage::put_file::LoreStoragePutFileItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_revision::interface::LoreString;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put-file".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;

                let payload = b"put_file remote upload payload".to_vec();
                let mut tempfile_handle = tempfile::Builder::new()
                    .prefix("lore-put-file-remote-")
                    .tempfile()
                    .expect("create tempfile");
                std::io::Write::write_all(&mut tempfile_handle, &payload).expect("write tempfile");
                let path = tempfile_handle.path().to_string_lossy().into_owned();
                let partition = Partition::from([0xb1u8; 16]);

                let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.id,
                            data.address,
                            data.error_code,
                        ));
                    }
                }));

                let item = LoreStoragePutFileItem {
                    id: 7,
                    partition,
                    context: Context::default(),
                    path: LoreString::from(path.as_str()),
                    remote_write: 1,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                let status = put_file::put_file(
                    LoreGlobalArgs::default(),
                    LoreStoragePutFileArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0, "put_file with remote_write=1 must succeed");

                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1, "exactly one PUT_ITEM_COMPLETE expected");
                let (id, address, code) = events[0];
                assert_eq!(id, 7);
                assert_eq!(code, LoreErrorCode::None);
                assert_ne!(address.hash, lore_base::types::Hash::default());

                let server_match = server
                    .backend_immutable
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("backend exist call");
                assert_eq!(
                    server_match,
                    StoreMatch::MatchFull,
                    "remote backend must hold the address after put_file with remote_write=1",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// `get_file` against a remote-only address (not in local cache) must fetch from the
    /// remote and write the bytes to the target file. Exercises the path that previously
    /// hardcoded `no_remote()` and would have failed with `ADDRESS_NOT_FOUND`.
    #[tokio::test]
    async fn get_file_falls_back_to_remote_on_local_miss() -> TestResult {
        use lore::storage::get_file;
        use lore::storage::get_file::LoreStorageGetFileArgs;
        use lore::storage::get_file::LoreStorageGetFileItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::FragmentFlags;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_revision::interface::LoreString;

        let execution = setup_execution("storage-remote-get-file".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;

                let payload = b"remote-only payload for get_file".to_vec();
                let partition = Partition::from([0xb2u8; 16]);
                let address = Address {
                    hash: lore_base::types::Hash::hash_buffer(&payload),
                    context: Context::from([0xb3u8; 16]),
                };

                // Seed the remote backend directly without touching the local cache —
                // the only reachable copy is on the server.
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                server
                    .backend_immutable
                    .clone()
                    .put(
                        partition,
                        address,
                        fragment,
                        Some(bytes::Bytes::from(payload.clone())),
                        false,
                    )
                    .await
                    .expect("backend seed put");

                let target_dir = tempfile::Builder::new()
                    .prefix("lore-get-file-remote-")
                    .tempdir()
                    .expect("create tempdir");
                let target_path_buf = target_dir.path().join("target");
                let target_path = target_path_buf.to_string_lossy().into_owned();

                let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.id,
                            data.address,
                            data.error_code,
                        ));
                    }
                }));

                let item = LoreStorageGetFileItem {
                    id: 11,
                    partition,
                    address,
                    path: LoreString::from(target_path.as_str()),
                    local_cache: 0,
                };
                let status = get_file::get_file(
                    LoreGlobalArgs::default(),
                    LoreStorageGetFileArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0, "get_file falling back to remote must succeed");

                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1, "exactly one GET_ITEM_COMPLETE expected");
                let (id, _addr, code) = events[0];
                assert_eq!(id, 11);
                assert_eq!(code, LoreErrorCode::None);

                let written = std::fs::read(&target_path).expect("read target file");
                assert_eq!(
                    written, payload,
                    "target file must hold the bytes fetched from the remote",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Open a remote-configured handle with `globals.offline=1`, then put with
    /// `remote_write=1`. The bound `offline` flag must silently suppress the upload — the
    /// server backend stays empty even though the API call returns success.
    async fn open_remote_handle_with_globals(server: &TestServer, globals: LoreGlobalArgs) -> u64 {
        let (sink, callback) = make_sink();
        let status = open::open(
            globals,
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                remote_config: LoreStorageRemoteConfig {
                    remote_url: LoreString::from(server.url.as_str()),
                },
                has_remote_config: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "open with bound globals must succeed");
        let events = sink.lock().unwrap().clone();
        take_opened(&events).expect("STORAGE_OPENED must fire on remote-configured open")
    }

    #[tokio::test]
    async fn bound_offline_suppresses_remote_upload_on_put() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-bound-offline".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let bound = LoreGlobalArgs {
                    offline: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let payload = b"bound-offline must not upload".to_vec();
                let partition = Partition::from([0xb1u8; 16]);

                let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.id,
                            data.address,
                            data.error_code,
                        ));
                    }
                }));

                let item = LoreStoragePutItem {
                    id: 7,
                    partition,
                    context: Context::default(),
                    data: LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 1,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                let status = put::put(
                    LoreGlobalArgs::default(),
                    LoreStoragePutArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;
                assert_eq!(
                    status, 0,
                    "put on bound-offline handle must succeed locally"
                );

                let events = captured.lock().unwrap().clone();
                let (_, address, code) = events[0];
                assert_eq!(code, LoreErrorCode::None);

                let server_match = server
                    .backend_immutable
                    .clone()
                    .exist(partition, address, StoreMatch::MatchFull)
                    .await
                    .expect("backend exist call");
                assert_eq!(
                    server_match,
                    StoreMatch::MatchNone,
                    "bound-offline handle must NOT push to the remote even with remote_write=1",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn bound_local_suppresses_remote_fetch_on_get_miss() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-local".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;

                // Seed the server with a payload that the bound-local handle's own local store
                // does not have. Without the binding, the get would fall through to remote and
                // succeed; the binding makes it fail.
                let payload_bytes = b"bound-local must miss".to_vec();
                let payload = Bytes::from(payload_bytes.clone());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);
                let address = Address {
                    hash,
                    context: Context::from([0xbcu8; 16]),
                };
                let partition = Partition::from([0xbdu8; 16]);
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                server
                    .backend_immutable
                    .clone()
                    .put(partition, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("seed server backend");

                let bound = LoreGlobalArgs {
                    local: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetItemComplete(data) = event {
                        captured_for_cb
                            .lock()
                            .unwrap()
                            .push((data.id, data.error_code));
                    }
                }));

                let item = LoreStorageGetItem {
                    id: 9,
                    partition,
                    address,
                    streaming: 0,
                    local_cache: 0,
                };
                // bound-local + non-streaming get of an address only present on the remote.
                // The handle must reject the miss locally rather than reaching out.
                let _ = get::get(
                    LoreGlobalArgs::default(),
                    LoreStorageGetArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    callback,
                )
                .await;

                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1, "exactly one GET_ITEM_COMPLETE expected");
                assert_eq!(
                    events[0].1,
                    LoreErrorCode::AddressNotFound,
                    "bound-local handle must NOT fetch remote on local miss; got {:?}",
                    events[0].1,
                );
                // Hash sanity
                assert_ne!(address.hash, Hash::default());

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn per_call_local_and_remote_combo_rejects_with_invalid_arguments() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-percall-conflict".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;

                let payload = b"per-call conflict".to_vec();
                let partition = Partition::from([0xbfu8; 16]);
                let item = LoreStoragePutItem {
                    id: 1,
                    partition,
                    context: Context::default(),
                    data: LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 1,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                // Per-call `local=1 && remote=1` must be rejected up front; status=1.
                let bad = LoreGlobalArgs {
                    local: 1,
                    remote: 1,
                    ..Default::default()
                };
                let status = put::put(
                    bad,
                    LoreStoragePutArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    None,
                )
                .await;
                assert_eq!(
                    status, 1,
                    "per-call local=1 && remote=1 must reject with status=1",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Helper: seed a payload on the server backend and return its `(partition, address)` so
    /// tests can drive remote-fetch behavior against a known-present remote address with no
    /// matching local entry.
    async fn seed_server_only(
        server: &TestServer,
        partition_byte: u8,
        payload_bytes: &[u8],
    ) -> (lore_base::types::Partition, lore_base::types::Address) {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        let payload = Bytes::copy_from_slice(payload_bytes);
        let hash = lore_storage::hash::hash_slice(payload_bytes);
        let address = Address {
            hash,
            context: Context::from([0xc0u8; 16]),
        };
        let partition = Partition::from([partition_byte; 16]);
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        server
            .backend_immutable
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("seed server backend");
        (partition, address)
    }

    /// Suppression-on-get: bound `offline=1` makes get against a server-only address miss
    /// rather than fetching. Mirrors the same shape of the `bound_local_*` test for
    /// completeness — `offline` and `local` produce equivalent storage-side behavior, so any
    /// future divergence between them surfaces here.
    #[tokio::test]
    async fn bound_offline_suppresses_remote_fetch_on_get_miss() -> TestResult {
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-offline-get".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let (partition, address) =
                    seed_server_only(&server, 0xc1, b"bound-offline get-miss target").await;
                let bound = LoreGlobalArgs {
                    offline: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetItemComplete(data) = event {
                        captured_for_cb
                            .lock()
                            .unwrap()
                            .push((data.id, data.error_code));
                    }
                }));
                let _ = get::get(
                    LoreGlobalArgs::default(),
                    LoreStorageGetArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageGetItem {
                            id: 21,
                            partition,
                            address,
                            streaming: 0,
                            local_cache: 0,
                        }]),
                    },
                    callback,
                )
                .await;
                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].1, LoreErrorCode::AddressNotFound);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Suppression on `get_metadata`: bound `local=1` makes `get_metadata` against a server-only
    /// address miss without consulting the remote.
    #[tokio::test]
    async fn bound_local_suppresses_remote_fetch_on_get_metadata_miss() -> TestResult {
        use lore::storage::get_metadata;
        use lore::storage::get_metadata::LoreStorageGetMetadataArgs;
        use lore::storage::get_metadata::LoreStorageGetMetadataItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-local-getmd".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let (partition, address) =
                    seed_server_only(&server, 0xc2, b"bound-local getmd-miss target").await;
                let bound = LoreGlobalArgs {
                    local: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetMetadataItemComplete(data) = event {
                        captured_for_cb
                            .lock()
                            .unwrap()
                            .push((data.id, data.error_code));
                    }
                }));
                let _ = get_metadata::get_metadata(
                    LoreGlobalArgs::default(),
                    LoreStorageGetMetadataArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageGetMetadataItem {
                            id: 22,
                            partition,
                            address,
                        }]),
                    },
                    callback,
                )
                .await;
                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].1, LoreErrorCode::AddressNotFound);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Suppression-on-upload: bound `offline=1` rejects upload up front rather than letting
    /// the call slip through.
    #[tokio::test]
    async fn bound_offline_rejects_upload_pre_dispatch() -> TestResult {
        use lore::storage::upload;
        use lore::storage::upload::LoreStorageUploadArgs;
        use lore::storage::upload::LoreStorageUploadItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-offline-upload".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let bound = LoreGlobalArgs {
                    offline: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let item = LoreStorageUploadItem {
                    id: 23,
                    partition: Partition::from([0xc3u8; 16]),
                    address: Address {
                        hash: Hash::from([0xeeu8; 32]),
                        context: Context::default(),
                    },
                };
                let status = upload::upload(
                    LoreGlobalArgs::default(),
                    LoreStorageUploadArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![item]),
                    },
                    None,
                )
                .await;
                assert_eq!(status, 1, "upload on bound-offline handle must reject");

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Suppression-on-obliterate: bound `local=1` makes obliterate run only the local leg;
    /// the remote leg is reported as `remote_skipped=1` (not `remote_success`).
    #[tokio::test]
    async fn bound_local_suppresses_remote_obliterate() -> TestResult {
        use lore::storage::obliterate;
        use lore::storage::obliterate::LoreStorageObliterateArgs;
        use lore::storage::obliterate::LoreStorageObliterateItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-local-obliterate".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let bound = LoreGlobalArgs {
                    local: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                #[allow(clippy::type_complexity)]
                let captured: Arc<Mutex<Vec<(u8, u8, u8, u8)>>> = Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageObliterateItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push((
                            data.local_success,
                            data.remote_success,
                            data.local_skipped,
                            data.remote_skipped,
                        ));
                    }
                }));
                let _ = obliterate::obliterate(
                    LoreGlobalArgs::default(),
                    LoreStorageObliterateArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageObliterateItem {
                            id: 24,
                            partition: Partition::from([0xc4u8; 16]),
                            address: Address {
                                hash: Hash::from([0x01u8; 32]),
                                context: Context::default(),
                            },
                        }]),
                    },
                    callback,
                )
                .await;
                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1);
                let (local_success, remote_success, local_skipped, remote_skipped) = events[0];
                // Local ran (absent address is idempotent success); remote was skipped.
                assert_eq!(local_success, 1);
                assert_eq!(local_skipped, 0);
                assert_eq!(remote_success, 0);
                assert_eq!(remote_skipped, 1);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Suppression-on-copy: bound `offline=1` degrades copy to local-only — the destination
    /// is not durably confirmed on the peer, but the call succeeds locally.
    #[tokio::test]
    async fn bound_offline_degrades_copy_to_local_only() -> TestResult {
        use lore::storage::copy;
        use lore::storage::copy::LoreStorageCopyArgs;
        use lore::storage::copy::LoreStorageCopyItem;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-bound-offline-copy".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let bound = LoreGlobalArgs {
                    offline: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                // Seed source partition locally via a put. With offline bound, the put is
                // local-only; the address is the same as if we'd hashed the bytes.
                let payload = b"bound-offline copy source".to_vec();
                let source_partition = Partition::from([0xc5u8; 16]);
                let source_context = Context::from([0xa5u8; 16]);
                let source_address = put_local_with_context(
                    handle_id,
                    source_partition,
                    source_context,
                    payload.as_slice(),
                )
                .await;

                // Target partition is different; copy under offline must run locally without
                // any wire contact.
                let target_partition = Partition::from([0xc6u8; 16]);
                let target_context = Context::from([0xa6u8; 16]);

                let captured: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageCopyItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push(data.error_code);
                    }
                }));
                let status = copy::copy(
                    LoreGlobalArgs::default(),
                    LoreStorageCopyArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageCopyItem {
                            id: 25,
                            source_partition,
                            source_address,
                            target_partition,
                            target_context,
                        }]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                assert_eq!(captured.lock().unwrap()[0], LoreErrorCode::None);
                drop(payload);

                // Server backend must NOT have received the copied address — proves remote
                // server-side copy was suppressed.
                let server_match = server
                    .backend_immutable
                    .clone()
                    .exist(
                        target_partition,
                        lore_base::types::Address {
                            hash: source_address.hash,
                            context: target_context,
                        },
                        StoreMatch::MatchFull,
                    )
                    .await
                    .expect("backend exist");
                assert_eq!(
                    server_match,
                    StoreMatch::MatchNone,
                    "bound-offline copy must not reach the remote",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Bound `remote=1`: read bypasses the local cache and reaches the remote even when the
    /// local side has the address. Seed both sides with the same address but different
    /// payloads so we can distinguish which side served the read.
    #[tokio::test]
    async fn bound_remote_bypasses_local_cache_on_get() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-remote-get".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;

                // Seed the server with a known payload first.
                let payload_bytes = b"served-from-remote".to_vec();
                let payload = Bytes::from(payload_bytes.clone());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);
                let address = Address {
                    hash,
                    context: Context::from([0xd0u8; 16]),
                };
                let partition = Partition::from([0xd1u8; 16]);
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                server
                    .backend_immutable
                    .clone()
                    .put(partition, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("seed server");

                // Bound-remote handle. The handle's local in-memory store is empty for the
                // address; with bypass-local semantics the read MUST reach the remote.
                let bound = LoreGlobalArgs {
                    remote: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let received: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                let received_for_cb = received.clone();
                let outcomes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                let outcomes_for_cb = outcomes.clone();
                let callback: LoreEventCallback =
                    Some(Box::new(move |event: &LoreEvent| match event {
                        LoreEvent::StorageGetData(data) => {
                            let slice = unsafe {
                                std::slice::from_raw_parts(
                                    data.bytes.ptr.cast::<u8>(),
                                    data.bytes.len,
                                )
                            };
                            received_for_cb.lock().unwrap().extend_from_slice(slice);
                        }
                        LoreEvent::StorageGetItemComplete(data) => {
                            outcomes_for_cb.lock().unwrap().push(data.error_code);
                        }
                        _ => {}
                    }));
                let status = get::get(
                    LoreGlobalArgs::default(),
                    LoreStorageGetArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageGetItem {
                            id: 26,
                            partition,
                            address,
                            streaming: 0,
                            local_cache: 0,
                        }]),
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                assert_eq!(outcomes.lock().unwrap()[0], LoreErrorCode::None);
                assert_eq!(*received.lock().unwrap(), payload_bytes);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Bound `remote=1`: copy items only attempt server-side. With a destination tuple that
    /// the server has NOT seen, tier-1 returns `NotFound` and the upload-fallback (tier 2)
    /// is suppressed — the result is `AddressNotFound` rather than a fallback success.
    #[tokio::test]
    async fn bound_remote_skips_copy_upload_fallback() -> TestResult {
        use lore::storage::copy;
        use lore::storage::copy::LoreStorageCopyArgs;
        use lore::storage::copy::LoreStorageCopyItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-remote-copy".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let bound = LoreGlobalArgs {
                    remote: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let captured: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageCopyItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push(data.error_code);
                    }
                }));
                let _ = copy::copy(
                    LoreGlobalArgs::default(),
                    LoreStorageCopyArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageCopyItem {
                            id: 27,
                            source_partition: Partition::from([0xd2u8; 16]),
                            source_address: Address {
                                hash: Hash::from([0xefu8; 32]),
                                context: Context::default(),
                            },
                            target_partition: Partition::from([0xd3u8; 16]),
                            target_context: Context::from([0xa7u8; 16]),
                        }]),
                    },
                    callback,
                )
                .await;
                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events[0],
                    LoreErrorCode::AddressNotFound,
                    "bound-remote copy must surface NotFound rather than fall through to upload",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Helper for the copy-suppression test: drive a local-only put against the handle with
    /// a caller-supplied context (the existing `put_local_via_handle` uses `Context::default`),
    /// then pull the resulting address out of the per-item event so subsequent ops can target
    /// it.
    async fn put_local_with_context(
        handle_id: u64,
        partition: lore_base::types::Partition,
        context: lore_base::types::Context,
        payload: &[u8],
    ) -> lore_base::types::Address {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_revision::event::LoreBytes;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<lore_base::types::Address>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push(data.address);
            }
        }));
        let item = LoreStoragePutItem {
            id: 0,
            partition,
            context,
            data: LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let status = put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "put failed");
        captured.lock().unwrap()[0]
    }

    // ----- Mutable store remote-path tests -----
    //
    // `globals.remote = 1` routes each op to the remote mutable store over the shared storage
    // session — the same session-based authz the immutable ops use. The default (local) routing
    // is covered by `storage_mutable_test`; here a local-vs-remote pair pins the explicit
    // selection.

    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_revision::event::LoreErrorCode;

    const REMOTE_KEY_TYPE: KeyType = KeyType::BranchLatestPointer;

    fn remote_globals() -> LoreGlobalArgs {
        LoreGlobalArgs {
            remote: 1,
            ..Default::default()
        }
    }

    /// Store a key-value pair through the handle, routed by `globals`. Returns the per-item
    /// `(id, error_code)` outcomes.
    async fn mutable_store_via_handle(
        handle_id: u64,
        globals: LoreGlobalArgs,
        partition: lore_base::types::Partition,
        key: Hash,
        value: Hash,
    ) -> (i32, Vec<(u64, lore_revision::event::LoreErrorCode)>) {
        use lore::storage::mutable_store;
        use lore::storage::mutable_store::LoreStorageMutableStoreArgs;
        use lore::storage::mutable_store::LoreStorageMutableStoreItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableStoreItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.error_code));
            }
        }));
        let status = mutable_store::mutable_store(
            globals,
            LoreStorageMutableStoreArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageMutableStoreItem {
                    id: 1,
                    partition,
                    key,
                    value,
                    key_type: REMOTE_KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Load a key through the handle, routed by `globals`. Returns the per-item `(value,
    /// error_code)`.
    async fn mutable_load_via_handle(
        handle_id: u64,
        globals: LoreGlobalArgs,
        partition: lore_base::types::Partition,
        key: Hash,
    ) -> (i32, Hash, lore_revision::event::LoreErrorCode) {
        use lore::storage::mutable_load;
        use lore::storage::mutable_load::LoreStorageMutableLoadArgs;
        use lore::storage::mutable_load::LoreStorageMutableLoadItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(Hash, LoreErrorCode)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableLoadItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.value, data.error_code));
            }
        }));
        let status = mutable_load::mutable_load(
            globals,
            LoreStorageMutableLoadArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageMutableLoadItem {
                    id: 7,
                    partition,
                    key,
                    key_type: REMOTE_KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        assert_eq!(events.len(), 1, "exactly one load complete event");
        (status, events[0].0, events[0].1)
    }

    #[tokio::test]
    async fn mutable_store_and_load_via_remote_round_trips() -> TestResult {
        let execution = setup_execution("storage-remote-mutable-roundtrip".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;
                let partition = lore_base::types::Partition::from([0xe1u8; 16]);
                let key = Hash::from([0xe2u8; 32]);
                let value = Hash::from([0xe3u8; 32]);

                let (status, completes) =
                    mutable_store_via_handle(handle_id, remote_globals(), partition, key, value)
                        .await;
                assert_eq!(status, 0, "remote mutable store must succeed");
                assert_eq!(completes, vec![(1, LoreErrorCode::None)]);

                // The value landed on the server's mutable store under the session's repository.
                let on_server = server
                    .backend_mutable
                    .clone()
                    .load(partition, key, REMOTE_KEY_TYPE)
                    .await
                    .expect("server backend must hold the stored key");
                assert_eq!(on_server, value, "server value must match the stored value");

                // Reading it back through the remote path returns the same value.
                let (load_status, loaded, code) =
                    mutable_load_via_handle(handle_id, remote_globals(), partition, key).await;
                assert_eq!(load_status, 0);
                assert_eq!(code, LoreErrorCode::None);
                assert_eq!(loaded, value);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn mutable_compare_and_swap_via_remote() -> TestResult {
        use lore::storage::mutable_compare_and_swap;
        use lore::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapArgs;
        use lore::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapItem;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-mutable-cas".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;
                let partition = lore_base::types::Partition::from([0xf1u8; 16]);
                let key = Hash::from([0xf2u8; 32]);
                let current = Hash::from([0xf3u8; 32]);
                let next = Hash::from([0xf4u8; 32]);

                let (status, _) =
                    mutable_store_via_handle(handle_id, remote_globals(), partition, key, current)
                        .await;
                assert_eq!(status, 0);

                let captured: Arc<Mutex<Vec<(Hash, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageMutableCompareAndSwapItemComplete(data) = event {
                        captured_for_cb
                            .lock()
                            .unwrap()
                            .push((data.previous, data.error_code));
                    }
                }));
                let cas_status = mutable_compare_and_swap::mutable_compare_and_swap(
                    remote_globals(),
                    LoreStorageMutableCompareAndSwapArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageMutableCompareAndSwapItem {
                            id: 1,
                            partition,
                            key,
                            expected: current,
                            value: next,
                            key_type: REMOTE_KEY_TYPE,
                        }]),
                    },
                    callback,
                )
                .await;
                assert_eq!(cas_status, 0, "remote CAS must succeed");
                let (previous, code) = captured.lock().unwrap()[0];
                assert_eq!(code, LoreErrorCode::None);
                assert_eq!(
                    previous, current,
                    "previous must equal the matched expected"
                );

                let on_server = server
                    .backend_mutable
                    .clone()
                    .load(partition, key, REMOTE_KEY_TYPE)
                    .await
                    .expect("server must hold the swapped value");
                assert_eq!(on_server, next, "server value must reflect the swap");

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn mutable_load_via_remote_missing_returns_not_found() -> TestResult {
        let execution = setup_execution("storage-remote-mutable-miss".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;
                let partition = lore_base::types::Partition::from([0xa1u8; 16]);
                let key = Hash::from([0xa2u8; 32]);

                let (status, value, code) =
                    mutable_load_via_handle(handle_id, remote_globals(), partition, key).await;
                assert_eq!(status, 1);
                assert_eq!(code, LoreErrorCode::AddressNotFound);
                assert_eq!(value, Hash::default());

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn mutable_default_routing_is_local_not_remote() -> TestResult {
        let execution = setup_execution("storage-remote-mutable-localdefault".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;
                let partition = lore_base::types::Partition::from([0xb1u8; 16]);
                let key = Hash::from([0xb2u8; 32]);
                let value = Hash::from([0xb3u8; 32]);

                // Default globals route to the handle's local mutable store, even though the
                // handle has a remote configured.
                let (status, completes) = mutable_store_via_handle(
                    handle_id,
                    LoreGlobalArgs::default(),
                    partition,
                    key,
                    value,
                )
                .await;
                assert_eq!(status, 0);
                assert_eq!(completes, vec![(1, LoreErrorCode::None)]);

                // The server's mutable store must not have seen it.
                let server_result = server
                    .backend_mutable
                    .clone()
                    .load(partition, key, REMOTE_KEY_TYPE)
                    .await;
                assert!(
                    server_result.is_err(),
                    "default-routed store must stay local, not reach the server",
                );

                // A local load sees it; a remote load misses.
                let (_s, local_value, local_code) =
                    mutable_load_via_handle(handle_id, LoreGlobalArgs::default(), partition, key)
                        .await;
                assert_eq!(local_code, LoreErrorCode::None);
                assert_eq!(local_value, value);
                let (_s2, _v, remote_code) =
                    mutable_load_via_handle(handle_id, remote_globals(), partition, key).await;
                assert_eq!(remote_code, LoreErrorCode::AddressNotFound);

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn mutable_list_via_remote_returns_error() -> TestResult {
        use lore::storage::mutable_list;
        use lore::storage::mutable_list::LoreStorageMutableListArgs;
        use lore::storage::mutable_list::LoreStorageMutableListItem;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-mutable-list".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let handle_id = open_remote_handle(&server).await;
                let partition = lore_base::types::Partition::from([0xc1u8; 16]);

                let entries: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
                let complete: Arc<Mutex<Option<LoreErrorCode>>> = Arc::new(Mutex::new(None));
                let entries_for_cb = entries.clone();
                let complete_for_cb = complete.clone();
                let callback: LoreEventCallback =
                    Some(Box::new(move |event: &LoreEvent| match event {
                        LoreEvent::StorageMutableListEntry(_) => {
                            *entries_for_cb.lock().unwrap() += 1;
                        }
                        LoreEvent::StorageMutableListItemComplete(data) => {
                            *complete_for_cb.lock().unwrap() = Some(data.error_code);
                        }
                        _ => {}
                    }));
                let status = mutable_list::mutable_list(
                    remote_globals(),
                    LoreStorageMutableListArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageMutableListItem {
                            id: 5,
                            partition,
                            key_type: REMOTE_KEY_TYPE,
                        }]),
                    },
                    callback,
                )
                .await;
                // Listing has no remote wire protocol, so a remote-targeted list is rejected up
                // front — the whole call fails with no per-item entry or terminal events.
                assert_eq!(status, 1, "remote list must fail the call");
                assert_eq!(
                    *entries.lock().unwrap(),
                    0,
                    "no entries on a rejected remote list"
                );
                assert!(
                    complete.lock().unwrap().is_none(),
                    "no per-item terminal event on a pre-dispatch rejection"
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }
}
