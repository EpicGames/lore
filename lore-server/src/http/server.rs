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
use lore_base::lore_spawn_net;
use lore_telemetry::http_tower_layer::HttpMetricsLayer;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use ring::hmac;
use tokio::net::TcpListener;
use tracing::info;

use super::health_check;
use super::presigned;
use super::security_headers::ContentTypeAllowlist;
use super::security_headers::ContentTypePolicy;
use super::security_headers::PolicyField;
use super::tracing::lore_http_tracing;
use crate::auth::jwt::JwtVerifier;
use crate::auth::jwt_axum_middleware::jwt_axum_verify_authorization;
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
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            request_timeout_seconds: 30,
            request_body_timeout_seconds: 30,
            ..Self::default()
        }
    }
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
fn describe_allowed_types(types: &[String]) -> String {
    if types.is_empty() {
        "<none>".to_string()
    } else {
        types.join(", ")
    }
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
        content_type_allowlist: ContentTypeAllowlist::try_from_policy(
            &settings.content_type_policy,
        )
        .map_err(|err| anyhow!("{} {err}", presign_content_type_field(err.field())))?,
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
            jwt_verifier,
            max_file_size: settings.max_file_size,
            presign_config,
        };

        let app = create_router(shared_state, health, &settings);

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| anyhow!("Failed to start HTTP server: {err}"))?;
        lore_spawn_net!(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal)
                .await
        })
        .await??;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::security_headers::DEFAULT_ALLOWED_CONTENT_TYPES;
    use super::*;

    /// 32 bytes of hex, the minimum `build_presign_config` accepts.
    const TEST_HMAC_KEY: &str = "32d0bd7711276da5a4d73e1211ba3884ad1819aa0b4727b8ee5d695e9c3199de";

    fn settings_with_policy(policy: ContentTypePolicy) -> PresignSettings {
        PresignSettings {
            hmac_key: Some(TEST_HMAC_KEY.to_string()),
            content_type_policy: policy,
            ..PresignSettings::default()
        }
    }

    fn types(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| (*t).to_string()).collect()
    }

    /// The test that catches a settings field added but never threaded into
    /// `PresignConfig`.
    #[test]
    fn build_presign_config_threads_extra_content_types() {
        let config = build_presign_config(&settings_with_policy(ContentTypePolicy {
            extra: types(&["application/zip"]),
            denied: Vec::new(),
        }))
        .expect("config should build")
        .expect("presign should be enabled");

        assert!(config.content_type_allowlist.is_allowed("application/zip"));
        assert!(config.content_type_allowlist.is_allowed("image/png"));
    }

    #[test]
    fn build_presign_config_threads_denied_content_types() {
        let config = build_presign_config(&settings_with_policy(ContentTypePolicy {
            extra: Vec::new(),
            denied: types(&["application/pdf"]),
        }))
        .expect("config should build")
        .expect("presign should be enabled");

        assert!(!config.content_type_allowlist.is_allowed("application/pdf"));
    }

    /// A browser-executable extra type stops the server from starting.
    #[test]
    fn build_presign_config_rejects_never_allowed_extra_type() {
        let result = build_presign_config(&settings_with_policy(ContentTypePolicy {
            extra: types(&["text/html"]),
            denied: Vec::new(),
        }));

        // Matched rather than `expect_err`, which would need `Debug` on
        // `PresignConfig` for tests alone.
        let Err(error) = result else {
            panic!("startup must fail on a browser-executable extra type");
        };

        assert!(
            error.to_string().contains("text/html"),
            "error must name the type, got: {error}"
        );
    }

    /// The policy must not accidentally enable the feature.
    #[test]
    fn build_presign_config_without_hmac_key_is_none() {
        let config = build_presign_config(&PresignSettings {
            hmac_key: None,
            content_type_policy: ContentTypePolicy {
                extra: types(&["application/zip"]),
                denied: Vec::new(),
            },
            ..PresignSettings::default()
        })
        .expect("config should build");

        assert!(config.is_none());
    }

    /// The derived `Default` must leave the policy empty, so programmatic
    /// construction resolves to the built-in set like an absent config key. Sets
    /// only `hmac_key`, so the policy comes from `Default` rather than the caller.
    #[test]
    fn presign_settings_default_resolves_to_builtin_set() {
        let config = build_presign_config(&PresignSettings {
            hmac_key: Some(TEST_HMAC_KEY.to_string()),
            ..PresignSettings::default()
        })
        .expect("config should build")
        .expect("presign should be enabled");

        let mut expected = types(DEFAULT_ALLOWED_CONTENT_TYPES);
        expected.sort_unstable();
        assert_eq!(config.content_type_allowlist.allowed_types(), expected);
    }

    /// An operator needs to know which of the two lists holds the bad entry.
    #[test]
    fn build_presign_config_error_names_the_extra_content_types_field() {
        let result = build_presign_config(&settings_with_policy(ContentTypePolicy {
            extra: types(&["text/html"]),
            denied: Vec::new(),
        }));

        let Err(error) = result else {
            panic!("startup must fail on a browser-executable extra type");
        };
        assert!(
            error
                .to_string()
                .contains("presigned_url_extra_content_types"),
            "error must name the field, got: {error}"
        );
    }

    #[test]
    fn build_presign_config_error_names_the_denied_content_types_field() {
        let result = build_presign_config(&settings_with_policy(ContentTypePolicy {
            extra: Vec::new(),
            denied: types(&["image/png,image/gif"]),
        }));

        let Err(error) = result else {
            panic!("startup must fail on a malformed denied entry");
        };
        assert!(
            error
                .to_string()
                .contains("presigned_url_denied_content_types"),
            "error must name the field, got: {error}"
        );
    }

    /// An empty set would otherwise render as nothing at all in the startup log.
    #[test]
    fn describe_allowed_types_marks_an_empty_set() {
        assert_eq!(describe_allowed_types(&[]), "<none>");
    }

    #[test]
    fn describe_allowed_types_joins_the_set() {
        assert_eq!(
            describe_allowed_types(&types(&["image/png", "text/plain"])),
            "image/png, text/plain"
        );
    }
}
