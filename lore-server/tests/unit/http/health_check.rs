// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use axum::routing;
use axum_test::TestServer;
use lore_server::auth::jwt::TokenVerification;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_server_is_up_and_listening() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    // Create the server and test the request
    let test_health = ServerHealth::new_without_availability(immutable_store.clone());
    let test_shared_state = ServerState {
        immutable_store,
        mutable_store,
        token_verification: TokenVerification::Ignore,
        repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
        max_file_size: 100,
        presign_config: None,
    };
    let settings = LoreHttpServerSettings::test_default();
    let app = create_router(test_shared_state, test_health, &settings);
    let test_server = TestServer::new(app).unwrap();

    let response = test_server.get("/health_check").await;

    assert_eq!(response.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn test_unavailable_store_server_is_not_healthy() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    // Create the server and test the request
    let test_health = ServerHealth {
        immutable_store: Arc::downgrade(&immutable_store),
        available: AtomicBool::new(false),
        interval_timeout: None,
        store_health_check: true,
    };
    let test_shared_state = ServerState {
        immutable_store,
        mutable_store,
        token_verification: TokenVerification::Ignore,
        repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
        max_file_size: 100,
        presign_config: None,
    };
    let settings = LoreHttpServerSettings {
        store_health_check: true,
        ..Default::default()
    };
    let app = create_router(test_shared_state, test_health, &settings);
    let test_server = TestServer::new(app).unwrap();

    let response = test_server.get("/health_check").await;

    assert_eq!(response.status_code(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_maintenance_mode_health_check_returns_ok() {
    // Simulate maintenance mode: no backing store, store_health_check disabled
    let health = Arc::new(ServerHealth {
        immutable_store: Weak::<lore_storage::LocalImmutableStore>::new(),
        available: AtomicBool::new(true),
        interval_timeout: None,
        store_health_check: false,
    });

    let app = axum::Router::new().route(
        "/health_check",
        routing::get(lore_server::http::health_check::handler).with_state(health),
    );
    let test_server = TestServer::new(app).unwrap();

    let response = test_server.get("/health_check").await;

    assert_eq!(response.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn health_only_excludes_content_and_uses_cached_store_status() {
    let (immutable_store, _mutable_store, execution) = test_store_create().await.unwrap();
    lore_base::runtime::LORE_CONTEXT.scope(execution, async move {
        for (available, expected) in [(true, StatusCode::OK), (false, StatusCode::SERVICE_UNAVAILABLE)] {
            let health = ServerHealth {
                immutable_store: Arc::downgrade(&immutable_store),
                available: AtomicBool::new(available),
                interval_timeout: None,
                store_health_check: true,
            };
            let app = lore_server::http::server::create_health_only_router(
                health, &LoreHttpServerSettings::test_default(),
            );
            let server = TestServer::new(app).unwrap();
            assert_eq!(server.get("/health_check").await.status_code(), expected);
            for path in [
                "/v1/repository/00000000000000000000000000000000/content/0000000000000000000000000000000000000000000000000000000000000000",
                "/v1/presigned/00000000000000000000000000000000/0000000000000000000000000000000000000000000000000000000000000000",
            ] {
                assert_eq!(server.get(path).await.status_code(), StatusCode::NOT_FOUND);
            }
        }
    }).await;
}

#[tokio::test]
async fn health_only_bypasses_invalid_presign_configuration() {
    let (immutable_store, mutable_store, execution) = test_store_create().await.unwrap();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let mut settings = LoreHttpServerSettings::test_default();
            settings.health_only = true;
            settings.presign.hmac_key = Some("invalid".into());
            let app = lore_server::http::server::build_router(
                &settings,
                immutable_store.clone(),
                mutable_store.clone(),
                TokenVerification::Ignore,
                Arc::new(AllowAllRepositoryAuthorizer),
            )
            .expect("health-only must not parse presign configuration");
            let server = TestServer::new(app).unwrap();
            assert_eq!(
                server.get("/health_check").await.status_code(),
                StatusCode::OK
            );
            settings.health_only = false;
            assert!(
                lore_server::http::server::build_router(
                    &settings,
                    immutable_store,
                    mutable_store,
                    TokenVerification::Ignore,
                    Arc::new(AllowAllRepositoryAuthorizer),
                )
                .is_err(),
                "full HTTP must still reject an invalid key"
            );
        })
        .await;
}

async fn check_http_server_lifetime(health_only: bool, cancel: bool) {
    let (immutable_store, mutable_store, execution) = test_store_create().await.unwrap();
    let weak = Arc::downgrade(&immutable_store);
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = reservation.local_addr().unwrap();
            drop(reservation);
            let settings = LoreHttpServerSettings {
                health_only,
                host: "127.0.0.1".into(),
                port: i32::from(address.port()),
                ..LoreHttpServerSettings::test_default()
            };
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let server = tokio_util::task::AbortOnDropHandle::new(lore_base::lore_spawn!(
                lore_server::http::server::LoreHttpServer::serve(
                    settings,
                    immutable_store,
                    mutable_store,
                    TokenVerification::Ignore,
                    Arc::new(AllowAllRepositoryAuthorizer),
                    async move {
                        let _ = shutdown_rx.await;
                    },
                )
            ));
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_millis(200))
                .build()
                .unwrap();
            let url = format!("http://{address}/health_check");
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    assert!(!server.is_finished(), "server exited before startup");
                    if let Ok(response) = client.get(&url).send().await {
                        assert_eq!(response.status(), StatusCode::OK);
                        response.bytes().await.unwrap();
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("health listener must start");
            assert!(weak.upgrade().is_some(), "serving must retain its store");
            let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
            connection
                .write_all(b"GET /health_check HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(connection.read_u8().await.unwrap());
                }
                assert!(headers.starts_with(b"HTTP/1.1 200"));
            })
            .await
            .expect("keep-alive connection must serve health before shutdown");

            if cancel {
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
            } else {
                shutdown_tx.send(()).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), server)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                check_closed_connection(&mut connection).await;
                return check_stopped_listener(&client, &url, &weak).await;
            }
            check_closed_connection(&mut connection).await;
            check_stopped_listener(&client, &url, &weak).await;
            // Keep the graceful signal pending until cancellation has been checked.
            drop(shutdown_tx);
        })
        .await;
}

async fn check_closed_connection(connection: &mut tokio::net::TcpStream) {
    let mut byte = [0];
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connection.read(&mut byte),
    )
    .await
    .expect("shutdown must close accepted keep-alive connections");
    match result {
        Ok(count) => assert_eq!(count, 0, "shutdown must close the connection"),
        Err(error) => assert!(matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        )),
    }
}

async fn check_stopped_listener(
    client: &reqwest::Client,
    url: &str,
    store: &Weak<dyn lore_storage::ImmutableStore>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store.upgrade().is_none() && client.get(url).send().await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shutdown must release the store and stop the listener");
}

#[tokio::test]
async fn health_only_server_retains_its_store_until_shutdown() {
    check_http_server_lifetime(true, false).await;
}

#[tokio::test]
async fn health_only_server_stops_when_cancelled() {
    check_http_server_lifetime(true, true).await;
}

#[tokio::test]
async fn full_http_server_still_shuts_down_gracefully() {
    check_http_server_lifetime(false, false).await;
}
