// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod presign_tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Partition;
    use lore_revision::fragment;
    use lore_server::http::server::LoreHttpServerSettings;
    use lore_server::http::server::PresignConfig;
    use lore_server::http::server::ServerHealth;
    use lore_server::http::server::ServerState;
    use lore_server::http::server::create_router;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use rand::random;

    use crate::setup_execution;

    fn test_presign_config() -> PresignConfig {
        let key_bytes = [0u8; 32];
        PresignConfig {
            hmac_key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes),
            key_id: "test_key_id_1234".to_string(),
            min_ttl_seconds: 1,
            default_ttl_seconds: 3600,
            max_ttl_seconds: 86400,
        }
    }

    /// End-to-end test: vend a presigned URL then redeem it via real HTTP requests.
    ///
    /// Unlike the unit tests in the server crate, which wrap the entire test body in
    /// `LORE_CONTEXT.scope()` and call handlers via `axum_test::TestServer` in
    /// the same task, this test drives a real `axum::serve` TCP listener.  Each
    /// request is handled in a fresh task with no outer execution context — the same
    /// conditions as production.  Any handler that calls into the store without first
    /// establishing its own `LORE_CONTEXT` scope will panic and fail this test.
    #[tokio::test]
    async fn presign_vend_and_redeem_round_trip() {
        // Set up stores and pre-load content within an execution context scope.
        let setup_execution = setup_execution("test".to_string());
        let (immutable_store, mutable_store, repo_hex, address_str, expected_payload) =
            LORE_CONTEXT
                .scope(setup_execution, async move {
                    let immutable: Arc<dyn ImmutableStore> =
                        lore_storage::local::immutable_store::create(
                            None::<&str>,
                            ImmutableStoreCreateOptions::none(),
                            false,
                            ImmutableStoreSettings {
                                implicit_durable_stored: true,
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();

                    let mutable: Arc<dyn MutableStore> =
                        lore_storage::local::mutable_store::create(
                            None::<&str>,
                            lore_storage::MutableStoreSettings::default(),
                            immutable.clone(),
                        )
                        .await
                        .unwrap();

                    let repository = random::<Partition>();
                    let (fragment_data, address, payload) = fragment::generate_random();

                    immutable
                        .clone()
                        .put(
                            repository,
                            address,
                            fragment_data,
                            Some(payload.clone()),
                            false,
                        )
                        .await
                        .unwrap();

                    (
                        immutable,
                        mutable,
                        format!("{repository}"),
                        format!("{address}"),
                        payload,
                    )
                })
                .await;

        // Bind the listener first so we know the port for the base URL.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let state = ServerState {
            immutable_store,
            mutable_store,
            jwt_verifier: None,
            max_file_size: 10 * 1024 * 1024,
            presign_config: Some(test_presign_config()),
        };

        let health = ServerHealth::new_without_availability(state.immutable_store.clone());
        let settings = LoreHttpServerSettings::default();
        let app = create_router(state, health, &settings);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        // Background server task in a test; LORE_CONTEXT propagation is unnecessary here.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    shutdown_rx.await.ok();
                })
                .await
                .unwrap();
        });

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
        }

        let client = reqwest::Client::new();

        let vend_url =
            format!("http://{addr}/v1/repository/{repo_hex}/content/{address_str}/presign");
        let vend_resp = client
            .post(&vend_url)
            .header("content-type", "application/json")
            .body(r#"{"ttl_seconds":3600}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(vend_resp.status(), 200);

        let vend_text = vend_resp.text().await.unwrap();
        let vend_body: serde_json::Value = serde_json::from_str(&vend_text).unwrap();
        let presigned_url_suffix = vend_body["url_suffix"].as_str().unwrap().to_string();

        let redeem_resp = client
            .get(format!("{base_url}{presigned_url_suffix}"))
            .send()
            .await
            .unwrap();

        assert_eq!(redeem_resp.status(), 200);

        let body = redeem_resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), &expected_payload[..]);

        let _ = shutdown_tx.send(());
    }
}
