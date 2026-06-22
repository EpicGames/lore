// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;

const MIN_HMAC_KEY_BYTES: usize = 32;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing;
use blake3;
use hex;
use lore_telemetry::http_tower_layer::HttpMetricsLayer;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use ring::hmac;
use tokio::net::TcpListener;
use tracing::info;

use super::health_check;
use super::presigned;
use super::tracing::lore_http_tracing;
use crate::auth::jwt::JwtVerifier;
use crate::auth::jwt_axum_middleware::jwt_axum_verify_authorization;
use crate::correlation::layer::CorrelationIdLayerBuilder;
use crate::http::repositories;

#[derive(Clone, Debug)]
pub struct LoreHttpServer {}

/// Configuration for the pre-signed URL vending and redemption feature.
/// Absent in test contexts; required for production (startup fails without it).
#[derive(Clone)]
pub struct PresignConfig {
    /// HMAC-SHA256 signing key.
    pub hmac_key: hmac::Key,
    /// First 16 hex chars of the BLAKE3 hash of the raw key bytes.
    pub key_id: String,
    pub min_ttl_seconds: u64,
    pub default_ttl_seconds: u64,
    pub max_ttl_seconds: u64,
}

#[derive(Clone)]
pub struct ServerState {
    pub immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    pub mutable_store: Arc<dyn lore_storage::MutableStore>,
    pub jwt_verifier: Option<JwtVerifier>,
    pub max_file_size: u64,
    pub presign_config: Option<PresignConfig>,
}

pub struct ServerHealth {
    pub immutable_store: Weak<dyn lore_storage::ImmutableStore>,
    pub available: AtomicBool,
    pub interval_timeout: Option<(Duration, Duration)>,
    pub store_health_check: bool,
}

impl ServerHealth {
    pub fn new_without_availability(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    ) -> Self {
        Self {
            immutable_store: Arc::downgrade(&immutable_store),
            available: AtomicBool::new(true),
            interval_timeout: None,
            store_health_check: false,
        }
    }
}

#[derive(Default)]
pub struct PresignSettings {
    pub hmac_key: Option<String>,
    pub min_ttl_seconds: u64,
    pub default_ttl_seconds: u64,
    pub max_ttl_seconds: u64,
}

#[derive(Default)]
pub struct LoreHttpServerSettings {
    pub host: String,
    pub port: i32,
    pub max_file_size: u64,
    pub request_timeout_seconds: u64,
    pub request_body_timeout_seconds: u64,
    pub available_interval_seconds: u64,
    pub available_timeout_seconds: u64,
    pub store_health_check: bool,
    pub presign: PresignSettings,
    /// User-agent filter applied to HTTP metrics labels.
    pub user_agent_filter: Arc<UserAgentFilter>,
}

// Expose a testable router factory
pub fn create_router(
    shared_state: ServerState,
    health: ServerHealth,
    settings: &LoreHttpServerSettings,
) -> Router {
    let repository_router: Router<ServerState> = repositories::create_router(shared_state.clone());
    let authenticated_router = Router::new()
        .nest("/repository", repository_router)
        .route_layer(middleware::from_fn_with_state(
            shared_state.clone(),
            jwt_axum_verify_authorization,
        ))
        // Do not process request that have more than 10 MiB in the body
        .layer(DefaultBodyLimit::max(shared_state.max_file_size as usize))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(settings.request_timeout_seconds),
        ))
        .layer(tower_http::timeout::RequestBodyTimeoutLayer::new(
            Duration::from_secs(settings.request_body_timeout_seconds),
        ))
        .with_state(shared_state.clone());

    let server_health = Arc::new(health);
    let unauthenticated_router = Router::new().route(
        "/health_check",
        routing::get(health_check::handler).with_state(server_health.clone()),
    );

    crate::store::spawn_immutable_store_availability_monitor(server_health);

    let mut router = Router::new()
        .merge(unauthenticated_router)
        .nest("/v1", authenticated_router);

    if shared_state.presign_config.is_some() {
        router = router.nest(
            "/v1/presigned",
            presigned::create_router(Arc::new(shared_state.clone())),
        );
    }

    router
        .layer(middleware::from_fn(lore_http_tracing))
        .layer(CorrelationIdLayerBuilder::new().with_http_tracer().build())
        .layer(HttpMetricsLayer::new(settings.user_agent_filter.clone()))
}

fn build_presign_config(settings: &PresignSettings) -> Result<Option<PresignConfig>> {
    let Some(key_hex) = settings.hmac_key.as_deref() else {
        return Ok(None);
    };

    let key_bytes = hex::decode(key_hex)
        .map_err(|e| anyhow::anyhow!("presigned_url_hmac_key is not valid hex: {e}"))?;

    if key_bytes.len() < MIN_HMAC_KEY_BYTES {
        anyhow::bail!(
            "presigned_url_hmac_key must be at least {MIN_HMAC_KEY_BYTES} bytes, got {}",
            key_bytes.len()
        );
    }

    let key_id = blake3::hash(&key_bytes).to_hex()[..16].to_string();
    let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);

    Ok(Some(PresignConfig {
        hmac_key,
        key_id,
        min_ttl_seconds: settings.min_ttl_seconds,
        default_ttl_seconds: settings.default_ttl_seconds,
        max_ttl_seconds: settings.max_ttl_seconds,
    }))
}

impl LoreHttpServer {
    /// Starts a minimal HTTP server that only serves the `/health_check` endpoint.
    ///
    /// Used during maintenance mode so that load balancers and monitoring systems
    /// can still reach the server. Always returns 200 OK (store health checks are
    /// disabled since the server is intentionally in a reduced state).
    pub async fn serve_maintenance(
        host: String,
        port: i32,
        user_agent_filter: Arc<UserAgentFilter>,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let addr = SocketAddr::from_str(format!("{host}:{port}").as_str())
            .map_err(|err| anyhow!("Failed to start maintenance HTTP server: {err}"))?;
        info!("Starting Lore maintenance HTTP Server: {}", &addr);

        let health = Arc::new(ServerHealth {
            immutable_store: Weak::<lore_storage::LocalImmutableStore>::new(),
            available: AtomicBool::new(true),
            interval_timeout: None,
            store_health_check: false,
        });

        let app = Router::new()
            .route(
                "/health_check",
                routing::get(health_check::handler).with_state(health),
            )
            .layer(HttpMetricsLayer::new(user_agent_filter));

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| anyhow!("Failed to start maintenance HTTP server: {err}"))?;
        axum::serve(listener, app)
            .with_graceful_shutdown(signal)
            .await?;

        Ok(())
    }

    pub async fn serve(
        settings: LoreHttpServerSettings,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        jwt_verifier: Option<JwtVerifier>,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let addr = SocketAddr::from_str(format!("{}:{}", settings.host, settings.port).as_str())
            .map_err(|err| anyhow!("Failed to start HTTP server: {err}"))?;
        info!(
            "Starting Lore HTTP Server: {}, Auth: {}",
            &addr,
            jwt_verifier.as_ref().map_or("no", |_| "yes")
        );

        let health = ServerHealth {
            immutable_store: Arc::downgrade(&immutable_store),
            available: AtomicBool::new(true),
            interval_timeout: if settings.available_interval_seconds > 0
                && settings.available_timeout_seconds > 0
            {
                Some((
                    Duration::from_secs(settings.available_interval_seconds),
                    Duration::from_secs(settings.available_timeout_seconds),
                ))
            } else {
                None
            },
            store_health_check: settings.store_health_check,
        };

        let presign_config = build_presign_config(&settings.presign)?;
        if let Some(cfg) = presign_config.as_ref() {
            info!("Presigned URL feature enabled (key_id: {})", cfg.key_id);
        } else {
            info!("Presigned URL feature disabled (presigned_url_hmac_key not configured)");
        }

        let shared_state = ServerState {
            immutable_store,
            mutable_store,
            jwt_verifier,
            max_file_size: settings.max_file_size,
            presign_config,
        };

        let app = create_router(shared_state, health, &settings);

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| anyhow!("Failed to start HTTP server: {err}"))?;
        axum::serve(listener, app)
            .with_graceful_shutdown(signal)
            .await?;

        Ok(())
    }
}
