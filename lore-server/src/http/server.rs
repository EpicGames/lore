// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::future::IntoFuture;
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
use lore_base::lore_spawn_net;
use lore_telemetry::http_tower_layer::HttpMetricsLayer;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use ring::hmac;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use super::health_check;
use super::presigned;
use super::security_headers::ContentTypeAllowlist;
use super::security_headers::ContentTypePolicy;
use super::security_headers::PolicyField;
use super::tracing::lore_http_tracing;
use crate::auth::jwt::TokenVerification;
use crate::auth::jwt_axum_middleware::jwt_axum_verify_authorization;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::correlation::layer::CorrelationIdLayerBuilder;
use crate::http::repositories;
use crate::util::core_hop::CoreHopLayer;

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
    /// Allowlist of `Content-Type` values that redeemed content may be served
    /// with. Disallowed types are rejected at mint and coerced to
    /// `application/octet-stream` at redeem.
    pub content_type_allowlist: ContentTypeAllowlist,
}

#[derive(Clone)]
pub struct ServerState {
    pub immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    pub mutable_store: Arc<dyn lore_storage::MutableStore>,
    pub token_verification: TokenVerification,
    pub repository_authorizer: Arc<dyn RepositoryAuthorizer>,
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
        ServerHealth {
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
    pub content_type_policy: ContentTypePolicy,
}

#[derive(Default)]
pub struct LoreHttpServerSettings {
    pub health_only: bool,
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

impl LoreHttpServerSettings {
    /// Settings suitable for unit tests: generous timeouts so a zero-duration
    /// `TimeoutLayer` (the result of `u64::default() == 0`) does not race
    /// against async handlers
    #[cfg(feature = "test-util")]
    pub fn test_default() -> Self {
        Self {
            request_timeout_seconds: 30,
            request_body_timeout_seconds: 30,
            ..Self::default()
        }
    }
}

#[lore_macro::test_pub]
fn apply_presigned_transport_limits(
    router: Router,
    max_file_size: u64,
    settings: &LoreHttpServerSettings,
) -> Router {
    // This route bypasses `authenticated_router`, so it needs its own transport limits.
    router
        .layer(DefaultBodyLimit::max(max_file_size as usize))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(settings.request_timeout_seconds),
        ))
        .layer(tower_http::timeout::RequestBodyTimeoutLayer::new(
            Duration::from_secs(settings.request_body_timeout_seconds),
        ))
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
        let presigned_router = apply_presigned_transport_limits(
            presigned::create_router(Arc::new(shared_state.clone())),
            shared_state.max_file_size,
            settings,
        );

        router = router.nest("/v1/presigned", presigned_router);
    }

    apply_http_middleware(router, settings)
}

fn apply_http_middleware(router: Router, settings: &LoreHttpServerSettings) -> Router {
    router
        .layer(middleware::from_fn(lore_http_tracing))
        .layer(CorrelationIdLayerBuilder::new().with_http_tracer().build())
        .layer(HttpMetricsLayer::new(settings.user_agent_filter.clone()))
        // Outermost, so everything inward runs on core: this router is served
        // from net.
        .layer(CoreHopLayer)
}

/// Maps a policy list to the config key that populates it.
fn presign_content_type_field(field: PolicyField) -> &'static str {
    match field {
        PolicyField::Extra => "presigned_url_extra_content_types",
        PolicyField::Denied => "presigned_url_denied_content_types",
    }
}

/// Renders the resolved allowlist for the startup log.
#[lore_macro::test_pub]
fn describe_allowed_types(types: &[String]) -> String {
    if types.is_empty() {
        "<none>".to_string()
    } else {
        types.join(", ")
    }
}

#[lore_macro::test_pub]
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
        content_type_allowlist: ContentTypeAllowlist::try_from_policy(
            &settings.content_type_policy,
        )
        .map_err(|err| anyhow!("{} {err}", presign_content_type_field(err.field())))?,
    }))
}

#[lore_macro::test_pub]
fn create_health_only_router(health: ServerHealth, settings: &LoreHttpServerSettings) -> Router {
    let health = Arc::new(health);
    crate::store::spawn_immutable_store_availability_monitor(health.clone());
    let router = Router::new().route(
        "/health_check",
        routing::get(health_check::handler).with_state(health),
    );
    apply_http_middleware(router, settings)
}

#[lore_macro::test_pub]
fn build_router(
    settings: &LoreHttpServerSettings,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    token_verification: TokenVerification,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
) -> Result<Router> {
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

    if settings.health_only {
        return Ok(create_health_only_router(health, settings));
    }

    let presign_config = build_presign_config(&settings.presign)?;
    if let Some(cfg) = presign_config.as_ref() {
        // Log the resolved set so operators can confirm what their config produced.
        info!(
            "Presigned URL feature enabled (key_id: {}, allowed content types: {})",
            cfg.key_id,
            describe_allowed_types(&cfg.content_type_allowlist.allowed_types())
        );
    } else {
        info!("Presigned URL feature disabled (presigned_url_hmac_key not configured)");
    }

    let shared_state = ServerState {
        immutable_store,
        mutable_store,
        token_verification,
        repository_authorizer,
        max_file_size: settings.max_file_size,
        presign_config,
    };

    Ok(create_router(shared_state, health, settings))
}

async fn serve_health_only(
    listener: TcpListener,
    app: Router,
    store_owner: Arc<dyn lore_storage::ImmutableStore>,
    signal: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // Cancellation signals Axum's connection tasks before the serving task releases the store.
    let server = lore_spawn_net!(async move {
        let shutdown_cancellation = cancellation.clone();
        let server = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                tokio::select! {
                    _ = signal => {},
                    _ = shutdown_cancellation.cancelled() => {},
                }
            })
            .into_future();
        tokio::pin!(server);
        let result = tokio::select! {
            result = &mut server => result,
            _ = cancellation.cancelled() => {
                // The caller can no longer join us; bound the remaining drain.
                if let Ok(result) = tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                    result
                } else {
                    warn!("HTTP connection drain timed out after cancellation");
                    Ok(())
                }
            },
        };
        drop(store_owner);
        result
    });
    server.await??;
    Ok(())
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
            .layer(HttpMetricsLayer::new(user_agent_filter))
            .layer(CoreHopLayer);

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| anyhow!("Failed to start maintenance HTTP server: {err}"))?;
        lore_spawn_net!(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal)
                .await
        })
        .await??;

        Ok(())
    }

    pub async fn serve(
        settings: LoreHttpServerSettings,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        token_verification: TokenVerification,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let addr = SocketAddr::from_str(format!("{}:{}", settings.host, settings.port).as_str())
            .map_err(|err| anyhow!("Failed to start HTTP server: {err}"))?;
        info!(
            "Starting Lore HTTP Server: {}, Auth: {}",
            &addr,
            token_verification.describe()
        );

        // The health-only router holds weak references; its serving task owns the store.
        let store_owner = settings.health_only.then(|| immutable_store.clone());
        let app = build_router(
            &settings,
            immutable_store,
            mutable_store,
            token_verification,
            repository_authorizer,
        )?;

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| anyhow!("Failed to start HTTP server: {err}"))?;
        if let Some(store_owner) = store_owner {
            return serve_health_only(listener, app, store_owner, signal).await;
        }

        lore_spawn_net!(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal)
                .await
        })
        .await??;

        Ok(())
    }
}
