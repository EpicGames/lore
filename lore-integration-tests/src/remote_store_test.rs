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
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::fragment;
    use lore_revision::lore::RepositoryId;
    use lore_revision::store::remote::RemoteImmutableStore;
    use lore_revision::store::remote::RemoteMutableStore;
    use lore_server::grpc::server::FeatureSettings;
    use lore_server::grpc::server::GrpcServerBuilder;
    use lore_server::hooks::HookDispatcher;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_storage::StoreMatch;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use rand::random;

    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    struct TestServer {
        immutable_store: Arc<RemoteImmutableStore>,
        mutable_store: Arc<RemoteMutableStore>,
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

        let url = format!("grpc://127.0.0.1:{}", addr.port());
        let immutable_store = Arc::new(RemoteImmutableStore::new(&url, None));
        let mutable_store = Arc::new(RemoteMutableStore::new(&url, None));

        TestServer {
            immutable_store,
            mutable_store,
            _shutdown: shutdown_tx,
        }
    }

    // ── Immutable Store Tests ──

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
                    .get(repository, address, StoreMatch::MatchFull)
                    .await?;

                assert_eq!(payload, got_payload);
                assert_eq!(fragment.size_payload, got_fragment.size_payload);
                assert_eq!(fragment.size_content, got_fragment.size_content);

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
                    .get(repository, address, StoreMatch::MatchFull)
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

                let match_result = server
                    .immutable_store
                    .clone()
                    .exist(repository, address, StoreMatch::MatchFull)
                    .await?;

                assert_eq!(StoreMatch::MatchFull, match_result);

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

                let match_result = server
                    .immutable_store
                    .clone()
                    .exist(repository, address, StoreMatch::MatchFull)
                    .await?;

                assert_eq!(StoreMatch::MatchNone, match_result);

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
                let results = server
                    .immutable_store
                    .clone()
                    .exist_batch(repository, &addresses, StoreMatch::MatchFull)
                    .await?;

                assert_eq!(5, results.len());
                assert_eq!(StoreMatch::MatchFull, results[0]);
                assert_eq!(StoreMatch::MatchFull, results[1]);
                assert_eq!(StoreMatch::MatchNone, results[2]);
                assert_eq!(StoreMatch::MatchFull, results[3]);
                assert_eq!(StoreMatch::MatchNone, results[4]);

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
                    .query(repository, address, StoreMatch::MatchFull)
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
                    .query(repository, address, StoreMatch::MatchFull)
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
                    .copy(repo_a, address, repo_b, address.context, false)
                    .await?;

                let (got_fragment, got_payload) = server
                    .immutable_store
                    .clone()
                    .get(repo_b, address, StoreMatch::MatchFull)
                    .await?;

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
