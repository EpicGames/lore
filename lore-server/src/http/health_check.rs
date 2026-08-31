// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::http::server::ServerHealth;

pub async fn handler(State(state): State<Arc<ServerHealth>>) -> impl IntoResponse {
    if state.store_health_check && !state.available.load(Ordering::Relaxed) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Weak;
    use std::sync::atomic::AtomicBool;

    use axum::http::StatusCode;
    use axum::routing;
    use axum_test::TestServer;

    use crate::http::server::LoreHttpServerSettings;
    use crate::http::server::ServerHealth;
    use crate::http::server::ServerState;
    use crate::http::server::create_router;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn test_server_is_up_and_listening() {
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        // Create the server and test the request
        let test_health = ServerHealth::new_without_availability(immutable_store.clone());
        let test_shared_state = ServerState {
            immutable_store,
            mutable_store,
            jwt_verifier: None,
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
            jwt_verifier: None,
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
            routing::get(super::handler).with_state(health),
        );
        let test_server = TestServer::new(app).unwrap();

        let response = test_server.get("/health_check").await;

        assert_eq!(response.status_code(), StatusCode::OK);
    }
}
