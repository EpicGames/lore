// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::env;

use config::Config;
use lore_base::runtime::TokioSettings;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::version::LORE_LIBRARY_VERSION;
use lore_revision::branch::CachedRevisionItem;
use lore_revision::branch::CachedRevisionListHeader;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::environment::EnvironmentConfig;
use lore_revision::util::time::RetrySettings;
use lore_storage::hash;
use lore_storage::hash::StringHash;
use lore_telemetry::TelemetryConfig;
use lore_telemetry::TraceConfigError;
use serde::Deserialize;

use crate::auth::jwk::JWKServiceSettings;
use crate::authnz::repository_authorizer::AuthorizerSelection;
use crate::authnz::repository_authorizer::select_repository_authorizer;
use crate::authnz::repository_catalog::select_repository_catalog;
use crate::grpc::server::FeatureSettings;
use crate::grpc::server::GrpcPublicServicesSettings;
use crate::hooks::HookSettings;
use crate::quic::client_monitor::default_quic_client_monitor_interval_secs;
use crate::store::replica_factory::ReplicaFactorySettings;
use crate::tls::CertificateSettings;
use crate::topology::TopologySettings;

#[derive(Clone, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Settings {
    pub environment: Option<EnvironmentConfig>,
    pub immutable_store: ImmutableStoreSettings,
    pub mutable_store: MutableStoreSettings,
    pub lock_store: Option<LockStoreSettings>,
    pub telemetry: Option<TelemetryConfig>,
    pub server: ServerSettings,
    pub feature: Option<FeatureSettings>,
    pub tokio: Option<TokioSettings>,
    pub notification: Option<NotificationSettings>,
    pub topology: Option<TopologySettings>,
    #[serde(default)]
    pub plugins: HashMap<String, toml::Value>,
    #[serde(default)]
    pub hooks: HashMap<String, HookSettings>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid printing the plugins and hooks in case they contain api keys
        // or other sensitive information, and will print all since they are
        // just toml bags of key-value pairs
        f.debug_struct("Settings")
            .field("environment", &self.environment)
            .field("immutable_store", &self.immutable_store)
            .field("mutable_store", &self.mutable_store)
            .field("lock_store", &self.lock_store)
            .field("telemetry", &self.telemetry)
            .field("server", &self.server)
            .field("feature", &self.feature)
            .field("tokio", &self.tokio)
            .field("notification", &self.notification)
            .field("topology", &self.topology)
            .field(
                "plugins",
                &self
                    .plugins
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .field(
                "hooks",
                &self
                    .hooks
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .finish()
    }
}

/// The built-in default configuration, baked into the binary at compile time.
///
/// This is the contents of `config/default.toml` and serves as the base layer
/// for every server invocation, so a stand alone server binary with no external
/// config starts with sensible defaults and needs no configuration files on disk.
const DEFAULT_CONFIG_TOML: &str = include_str!("../config/default.toml");

/// The on-disk config directory used when no `config_path` is supplied (via
/// `--config` / `LORE_CONFIG_PATH`). Files in it are optional, so a missing
/// directory just leaves the server running on its built-in defaults.
const DEFAULT_CONFIG_DIR: &str = "lore-server/config";

impl Settings {
    /// Load settings, layering optional on-disk overrides over the built-in
    /// defaults baked into the binary.
    ///
    /// Layering order (later sources win):
    /// 1. The built-in [`DEFAULT_CONFIG_TOML`] (always present).
    /// 2. The optional on-disk `default.toml` from the config directory.
    ///    When present it lets operators tune the compiled-in defaults without
    ///    requiring a rebuild.
    /// 3. The optional files `<environment>.toml`,
    ///    `<environment>_<region>.toml`, and `local.toml` from the config
    ///    directory. The directory comes from `config_path` (via `--config` /
    ///    `LORE_CONFIG_PATH`) when supplied, otherwise it falls back to
    ///    [`DEFAULT_CONFIG_DIR`].
    /// 4. Environment variables prefixed with `LORE__`.
    ///
    /// Every on-disk file is optional, so the server still starts from the
    /// built-in defaults (and environment variables) even when the config
    /// directory is absent.
    pub fn load(
        config_path: Option<&str>,
        environment: Option<&str>,
    ) -> Result<(Self, StringHash), config::ConfigError> {
        println!("Server version: {}", LORE_LIBRARY_VERSION.as_str());

        let environment = environment.unwrap_or("local");
        println!("Using environment: {environment}");

        // Start from the defaults baked into the binary so the server can run
        // with no configuration files present at all.
        let mut settings_builder = Config::builder().add_source(config::File::from_str(
            DEFAULT_CONFIG_TOML,
            config::FileFormat::Toml,
        ));

        // Resolve the config directory: use the path supplied on the command
        // line (or via LORE_CONFIG_PATH) when present, otherwise fall back to
        // the default `lore-server/config` directory. Every on-disk file below
        // is optional: missing files (or a missing directory) are silently skipped.
        let config_path = config_path.unwrap_or(DEFAULT_CONFIG_DIR);
        println!("Using config path: {config_path}");

        // Layer an optional on-disk default.toml for env/region agnostic settings
        // extending the built-in defaults
        settings_builder = settings_builder
            .add_source(config::File::with_name(&format!("{config_path}/default")).required(false));
        settings_builder = settings_builder.add_source(
            config::File::with_name(&format!("{config_path}/{environment}")).required(false),
        );
        if let Ok(instance_region) = env::var("LORE_PLATFORM_REGION") {
            settings_builder = settings_builder.add_source(
                config::File::with_name(&format!("{config_path}/{environment}_{instance_region}"))
                    .required(false),
            );
        }
        settings_builder = settings_builder
            .add_source(config::File::with_name(&format!("{config_path}/local")).required(false));

        settings_builder =
            settings_builder.add_source(config::Environment::with_prefix("lore").separator("__"));

        let settings = settings_builder.build()?;
        let settings: Settings = settings.try_deserialize()?;
        validate_trace_config(&settings)?;
        validate_feature_config(&settings)?;
        validate_auth_config(&settings)?;
        let settings_string = format!("{settings:?}");
        let settings_hash = hash::hash_string(&settings_string);

        // Logger isn't configured yet.
        println!("Loaded config: {settings_string}");

        Ok((settings, settings_hash))
    }
}

/// Missing `jwt_issuer` / `jwt_audience` under `[server.auth]` fails
/// deserialization. Calls repository authorizer selection logic to verify
/// that the configuration combination is valid for authorization.
#[lore_macro::test_pub]
fn validate_auth_config(settings: &Settings) -> Result<(), config::ConfigError> {
    let auth = settings.server.auth.as_ref();
    let auth_url = settings
        .environment
        .as_ref()
        .and_then(|environment| environment.endpoint.as_ref())
        .and_then(|endpoint| endpoint.auth_url.as_deref());
    // Run the authorizer and catalog selection at load, so a refused pairing
    // bails here, before any initialization, instead of at server startup.
    let authorizer = select_repository_authorizer(auth, auth_url)
        .map_err(|err| config::ConfigError::Message(err.to_string()))?;
    select_repository_catalog(auth, auth_url)
        .map_err(|err| config::ConfigError::Message(err.to_string()))?;
    let Some(auth) = auth else {
        return Ok(());
    };
    if auth.jwt_issuer.is_empty() {
        return Err(config::ConfigError::Message(
            "server.auth.jwt_issuer must not be empty".to_string(),
        ));
    }
    if auth.jwt_audience.is_empty() {
        return Err(config::ConfigError::Message(
            "server.auth.jwt_audience must not be empty".to_string(),
        ));
    }
    if let Some(accepted) = auth.jwt_typ.as_ref() {
        if accepted.is_empty() {
            return Err(config::ConfigError::Message(
                "server.auth.jwt_typ must name at least one type. Omit it to skip the check."
                    .to_string(),
            ));
        }
        if accepted
            .iter()
            .any(|typ| typ.is_empty() || typ.chars().any(char::is_whitespace))
        {
            return Err(config::ConfigError::Message(
                "server.auth.jwt_typ entries must be media types: not empty, no whitespace"
                    .to_string(),
            ));
        }
    }
    validate_oidc_config(auth, authorizer)
}

/// Tier 1 and Tier 2 have no login path besides OIDC, so they require
/// `[server.auth.oidc]`. If grpc auth service is configured, it is optional.
fn validate_oidc_config(
    auth: &AuthSettings,
    selection: AuthorizerSelection,
) -> Result<(), config::ConfigError> {
    let error = |message: &str| Err(config::ConfigError::Message(message.to_string()));
    let Some(oidc) = auth.oidc.as_ref() else {
        return match selection {
            AuthorizerSelection::GlobalGrants | AuthorizerSelection::ResourceGrants => error(
                "[server.auth] without [environment.endpoint] auth_url authorizes OIDC tokens, \
                 so clients need [server.auth.oidc] to log in.",
            ),
            AuthorizerSelection::AllowAll | AuthorizerSelection::AuthClient => Ok(()),
        };
    };
    let is_issuer_url = auth.jwt_issuer.first().is_some_and(|issuer| {
        reqwest::Url::parse(issuer).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
    });
    if !is_issuer_url {
        return error(
            "server.auth.oidc requires the first server.auth.jwt_issuer entry to be the \
             provider's issuer URL",
        );
    }
    if oidc.client_id.is_empty() {
        return error("server.auth.oidc.client_id must not be empty");
    }
    let is_set = |value: &Option<String>| value.as_ref().is_some_and(|value| !value.is_empty());
    let has_template = match (
        is_set(&oidc.resource_template),
        is_set(&oidc.scope_template),
    ) {
        (true, true) => {
            return error(
                "server.auth.oidc.resource_template and server.auth.oidc.scope_template are \
                 both set. Set one",
            );
        }
        (resource, scope) => resource || scope,
    };
    if is_set(&oidc.token_exchange_issuer) && !has_template {
        return error(
            "server.auth.oidc.token_exchange_issuer is set without \
             server.auth.oidc.resource_template or server.auth.oidc.scope_template",
        );
    }
    match (selection, has_template) {
        (AuthorizerSelection::ResourceGrants, false) => error(
            "server.auth.resource_claim authorizes per partition, but neither \
             server.auth.oidc.resource_template nor server.auth.oidc.scope_template is set, so \
             clients never ask for partition-scoped tokens. Set one, or remove resource_claim",
        ),
        (AuthorizerSelection::GlobalGrants, true) => error(
            "server.auth.oidc.resource_template or server.auth.oidc.scope_template is set, but \
             without server.auth.resource_claim the server never reads partition-scoped tokens. \
             Set resource_claim, or remove the template",
        ),
        _ => Ok(()),
    }
}

fn validate_trace_config(settings: &Settings) -> Result<(), config::ConfigError> {
    if let Some(traces) = settings.telemetry.as_ref().and_then(|t| t.traces.as_ref()) {
        traces.validate().map_err(trace_config_error_to_config)?;
    }
    Ok(())
}

/// A cached revision-list blob is a single `CachedRevisionListHeader`
/// followed by `history_step_size` packed `CachedRevisionItem`s. The
/// whole blob is written to the immutable store as one fragment, so it
/// must fit strictly under `FRAGMENT_SIZE_THRESHOLD` — otherwise pushes
/// would silently fail to materialize cache entries.
fn validate_feature_config(settings: &Settings) -> Result<(), config::ConfigError> {
    let history_step_size = settings
        .feature
        .as_ref()
        .and_then(|f| f.history_step_size)
        .unwrap_or(DEFAULT_HISTORY_STEP_SIZE);
    let header_size = std::mem::size_of::<CachedRevisionListHeader>();
    let item_size = std::mem::size_of::<CachedRevisionItem>();
    let blob_size =
        header_size.saturating_add((history_step_size as usize).saturating_mul(item_size));
    if blob_size >= FRAGMENT_SIZE_THRESHOLD {
        return Err(config::ConfigError::Message(format!(
            "feature.history_step_size ({history_step_size}) × CachedRevisionItem size \
             ({item_size}) + header ({header_size}) = {blob_size} bytes does not fit under the \
             fragment threshold ({FRAGMENT_SIZE_THRESHOLD}); reduce history_step_size",
        )));
    }
    Ok(())
}

fn trace_config_error_to_config(err: TraceConfigError) -> config::ConfigError {
    match err {
        TraceConfigError::OutOfRange { field, value } => config::ConfigError::Message(format!(
            "telemetry.traces.{field} value {value} is outside [0.0, 1.0]"
        )),
    }
}

///
/// Server-related settings
///

#[serde_with::serde_as]
#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct AuthSettings {
    /// Optional JWK override. Verification is enabled by `[server.auth]`.
    /// If this or its `endpoint` is absent, the JWKS endpoint is
    /// resolved through OIDC discovery against `jwt_issuer`.
    pub jwk: Option<JWKServiceSettings>,
    /// The accepted `aud` values.
    pub jwt_audience: Vec<String>,
    /// The accepted `iss` values. A bare string still parses, so existing
    /// configs need no edit. Two entries is for the length of an issuer's
    /// cutover — accepting tokens minted under both the old and the new `iss`
    /// while they are both in flight — and one entry otherwise. The list is not
    /// for discovering several providers: discovery resolves against the first
    /// entry, and two entries with different discovery documents is a
    /// configuration error.
    #[serde_as(as = "serde_with::OneOrMany<_, serde_with::formats::PreferMany>")]
    pub jwt_issuer: Vec<String>,
    /// The accepted `typ` header values, as a bare string or a list. Absent,
    /// the header is not checked, which every token the legacy auth service
    /// issues needs. `"at+jwt"` is the RFC 9068 §4 rule. Values compare as
    /// media types: case-insensitively, and with or without the
    /// `application/` prefix.
    #[serde_as(as = "Option<serde_with::OneOrMany<_, serde_with::formats::PreferMany>>")]
    #[serde(default)]
    pub jwt_typ: Option<Vec<String>>,
    /// Dotted path of the JWT claim carrying the caller's allowed actions.
    pub permission_claim: Option<String>,
    /// Dotted path of the claim carrying per-repository resource grants.
    /// If this is set, enables the granular `ResourceGrantsAuthorizer`.
    pub resource_claim: Option<String>,
    /// The field inside each resource entry naming the resource, for
    /// providers whose entry shape cannot be changed (Keycloak's UMA
    /// `permissions` entries carry `rsname`, for example).
    #[serde(default = "AuthSettings::default_resource_id_claim")]
    pub resource_id_claim: String,
    /// Template that renders a repository id into the corresponding resource
    /// name.
    #[serde(default = "AuthSettings::default_resource_id_template")]
    pub resource_id_template: String,
    /// The resource name that matches every repository.
    #[serde(default = "AuthSettings::default_resource_wildcard")]
    pub resource_wildcard: String,
    /// The claim recorded and compared as the caller's identity.
    #[serde(default = "AuthSettings::default_identity_claim")]
    pub identity_claim: String,
    /// What the repository listing answers for an authenticated caller with
    /// no explicit grant. Gates listing of the IDs only, never grants
    /// access to the contents. Consulted by the `baseline` catalog only.
    #[serde(default)]
    pub baseline_access: BaselineAccess,
    /// Which catalog answers the repository listing. Absent: `auth_service`
    /// when `[environment.endpoint] auth_url` is set, `baseline` otherwise.
    pub repository_catalog: Option<RepositoryCatalogMode>,
    /// The `UrcAuthApi` endpoint the `auth_service` catalog asks. Absent:
    /// `[environment.endpoint] auth_url`.
    pub repository_catalog_url: Option<String>,
    /// OIDC provider settings advertised for clients.
    pub oidc: Option<OidcSettings>,
}

/// What the environment service advertises about the OIDC provider,
/// beyond what the server reads from `jwt_issuer`'s discovery document and the
/// rest of `[server.auth]`.
#[derive(Clone, Debug, Deserialize)]
pub struct OidcSettings {
    /// The public client ID clients present to the provider.
    pub client_id: String,
    /// Default scopes clients request at login.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Whether a client should default to OIDC login or use gRPC auth at
    /// at `[environment.endpoint] auth_url`.
    #[serde(default)]
    pub preferred: bool,
    /// Template string for mapping a partition as an RFC 8707 resource, with
    /// `{id}` standing for the partition ID. Tier 2.
    pub resource_template: Option<String>,
    /// Template string for mapping a partition as a scope value, with
    /// `{id}` standing for the partition ID. Tier 2.
    pub scope_template: Option<String>,
    /// The issuer of the RFC 8693 token-exchange endpoint, if using a separate
    /// token service. If not set, uses the first `jwt_issuer` entry. Tier 2.
    pub token_exchange_issuer: Option<String>,
}

impl AuthSettings {
    fn default_resource_id_template() -> String {
        crate::auth::jwt::DEFAULT_RESOURCE_ID_TEMPLATE.to_string()
    }

    fn default_resource_id_claim() -> String {
        "resource_id".to_string()
    }

    fn default_resource_wildcard() -> String {
        crate::auth::jwt::DEFAULT_RESOURCE_WILDCARD.to_string()
    }

    fn default_identity_claim() -> String {
        "sub".to_string()
    }
}

/// What `list_repositories` answers for an authenticated caller holding no
/// explicit grant.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BaselineAccess {
    /// Every partition the server holds is listed. Opt-in: it discloses
    /// partition identifiers to every authenticated caller.
    Reachable,
    /// Nothing is listed without a grant. The default, so the disclosing
    /// option is an explicit choice.
    #[default]
    Denied,
}

/// Which catalog answers the repository listing.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryCatalogMode {
    /// Answer per `baseline_access` from the server's own store.
    Baseline,
    /// Ask a `UrcAuthApi` service's `LookupUserPermissions`, forwarding the
    /// caller's token.
    AuthService,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct GrpcSettings {
    /// Whether to start this gRPC endpoint. Defaults to `false`
    #[serde(default)]
    pub enabled: bool,
    pub certificate: Option<CertificateSettings>,
    pub host: String,
    pub port: i32,
    pub http2_keepalive_interval_seconds: Option<u64>,
    pub http2_keepalive_timeout_seconds: Option<u64>,
    /// Keep below the ALB timeout to ensure we gracefully observe stuck requests
    /// rather than clients receive a 504 response from the ALB
    pub request_handler_timeout_seconds: u64,
    /// Ceiling on the partition-access authorization check that precedes a
    /// handler, covering the online authorizer call it may make. Sized for
    /// reaching the authorizer rather than for a whole request, so it is well
    /// below `request_handler_timeout_seconds`.
    #[serde(default = "default_authorization_timeout_seconds")]
    pub authorization_timeout_seconds: u64,
    /// Require client certificates (mTLS): `true` demands a full mTLS triple,
    /// `false` accepts unverified clients.
    #[serde(default = "default_verify_client_certs")]
    pub verify_client_certs: bool,
}

fn default_verify_client_certs() -> bool {
    true
}

pub(crate) fn default_authorization_timeout_seconds() -> u64 {
    10
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    #[allow(dead_code)]
    pub certificate: Option<CertificateSettings>,
    pub enabled: bool,
    pub host: String,
    pub max_file_size: u64,
    pub port: i32,
    pub request_timeout_seconds: u64,
    pub request_body_timeout_seconds: u64,
    pub available_interval_seconds: u64,
    pub available_timeout_seconds: u64,
    pub store_health_check: bool,
    pub presigned_url_hmac_key: Option<String>,
    #[serde(default = "HttpSettings::default_presigned_url_min_ttl_seconds")]
    pub presigned_url_min_ttl_seconds: u64,
    #[serde(default = "HttpSettings::default_presigned_url_default_ttl_seconds")]
    pub presigned_url_default_ttl_seconds: u64,
    #[serde(default = "HttpSettings::default_presigned_url_max_ttl_seconds")]
    pub presigned_url_max_ttl_seconds: u64,
    /// Added to the built-in set of `Content-Type` values redeemed content may be
    /// served with. Browser-executable types are refused at startup.
    #[serde(default)]
    pub presigned_url_extra_content_types: Vec<String>,
    /// Removed from that set, after the extra types.
    #[serde(default)]
    pub presigned_url_denied_content_types: Vec<String>,
}

impl HttpSettings {
    fn default_presigned_url_min_ttl_seconds() -> u64 {
        1
    }

    fn default_presigned_url_default_ttl_seconds() -> u64 {
        3600
    }

    fn default_presigned_url_max_ttl_seconds() -> u64 {
        86400
    }
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct QuicSettings {
    /// Whether to start this QUIC endpoint. Defaults to `false`
    #[serde(default)]
    pub enabled: bool,
    pub certificate: Option<CertificateSettings>,
    /// Require client certificates (mTLS): `true` demands a full mTLS triple,
    /// `false` accepts unverified clients.
    #[serde(default = "default_verify_client_certs")]
    pub verify_client_certs: bool,
    pub host: String,
    pub idle_timeout: Option<u64>,
    pub keep_alive: Option<u64>,
    pub max_bidi_streams: Option<u64>,
    pub num_listeners: u8,
    pub port: i32,
    pub transport_bits_per_second: Option<usize>,
    pub transport_rtt: Option<usize>,
    /// Keep below a threshold for whatever Load Balancer sits in front of the server
    /// or is expecting responses. If request handlers exceed this reasonable threshold
    /// then assume something has gone wrong and return a timeout response so we can get metrics
    /// and clients don't hang forever
    pub handler_timeout_seconds: Option<u64>,
    /// How many inflight messages are allowed per QUIC stream. With `max_bidi_streams` streams
    /// this is the per-connection parallelism, so a value of 500 over 8 streams allows 4000
    /// commands in flight per connection.
    pub stream_message_limit: Option<usize>,
    /// Hard ceiling on requests in handling per connection, counted across all its streams and
    /// including those still waiting for a stream permit. Over it, the server answers `SlowDown`
    /// immediately rather than waiting. Defaults to `stream_message_limit * max_bidi_streams`.
    pub connection_inflight_limit: Option<usize>,
    /// How long a request may wait for one of the `stream_message_limit` permits before the
    /// server answers `SlowDown`. Defaults to roughly one round trip, 100ms.
    pub permit_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct ServerSettings {
    pub auth: Option<AuthSettings>,
    pub grpc: Option<GrpcSettings>,
    /// One block per public gRPC service; an absent table enables every
    /// service.
    #[serde(default)]
    pub grpc_public_services: GrpcPublicServicesSettings,
    pub grpc_internal: Option<GrpcSettings>,
    pub http: Option<HttpSettings>,
    // the public facing QUIC server settings
    pub quic: Option<QuicSettings>,
    // the internal-only QUIC server settings
    pub quic_internal: Option<QuicSettings>,
    /// Seconds to wait for existing connections to close gracefully after shutdown signal.
    #[serde(default = "default_connection_close_timeout")]
    pub connection_close_timeout_seconds: u16,
    /// Seconds to wait for async runtime to shut down after connections are closed.
    #[serde(
        default = "default_runtime_shutdown_timeout",
        alias = "shutdown_delay_seconds"
    )]
    pub runtime_shutdown_timeout_seconds: u16,
    #[serde(default)]
    pub user_agent: UserAgentSettings,
    /// Disk space monitoring for the local store paths.
    #[serde(default)]
    pub local_store_monitor: LocalStoreMonitorSettings,
}

/// Periodic monitoring of the disk space available to the local store.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LocalStoreMonitorSettings {
    /// Seconds between checks. Zero turns monitoring off.
    pub check_interval_seconds: u64,
    /// Available space, in bytes, below which a warning is logged.
    pub low_space_threshold_bytes: u64,
}

impl Default for LocalStoreMonitorSettings {
    fn default() -> Self {
        Self {
            check_interval_seconds: 30,
            low_space_threshold_bytes: 10 * 1024 * 1024 * 1024,
        }
    }
}

// For when this server acts as a client to another server's Internal port
#[derive(Clone, Debug, Deserialize)]
pub struct GrpcInternalClientSettings {
    pub url: String,
    pub certs: Option<CertificateSettings>,
    /// Ceiling on the TCP connect. Covers neither DNS nor the TLS handshake.
    #[serde(default = "GrpcInternalClientSettings::default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// Deadline for each request on the channel. Keep below the
    /// `request_handler_timeout_seconds` of the endpoint whose handler issues it.
    #[serde(default = "GrpcInternalClientSettings::default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    #[serde(default = "GrpcInternalClientSettings::default_tcp_keepalive_seconds")]
    pub tcp_keepalive_seconds: u64,
    /// HTTP/2 keep-alive PING interval, sent while the channel is idle. Keep below
    /// the idle timeout of anything on the path that reaps idle connections.
    #[serde(default = "GrpcInternalClientSettings::default_http2_keepalive_interval_seconds")]
    pub http2_keepalive_interval_seconds: u64,
    /// How long a keep-alive PING may go unanswered before the connection is
    /// dropped.
    #[serde(default = "GrpcInternalClientSettings::default_http2_keepalive_timeout_seconds")]
    pub http2_keepalive_timeout_seconds: u64,
}

impl GrpcInternalClientSettings {
    fn default_connect_timeout_seconds() -> u64 {
        5
    }

    fn default_request_timeout_seconds() -> u64 {
        40
    }

    fn default_tcp_keepalive_seconds() -> u64 {
        30
    }

    fn default_http2_keepalive_interval_seconds() -> u64 {
        20
    }

    fn default_http2_keepalive_timeout_seconds() -> u64 {
        10
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UserAgentSettings {
    #[serde(default)]
    pub user_agent_patterns: Vec<String>,
    #[serde(default)]
    pub unknown_user_agent_sample_rate: f64,
}

fn default_connection_close_timeout() -> u16 {
    5
}

fn default_runtime_shutdown_timeout() -> u16 {
    25
}

///
/// Storage-related settings
///

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct CompositeStoreSettings {
    pub durable: Option<CompositeSubStoreSettings>,
    pub local: CompositeSubStoreSettings,
    pub replica: Option<Vec<CompositeSubStoreSettings>>,
    pub replica_factory: Option<ReplicaFactorySettings>,
    pub cache_metadata: Option<bool>,
    pub cache_metadata_semaphore_size: Option<usize>,
    /// Whether a copy is recorded in the local store and at the write replicas as well as in the
    /// durable store. Absent enables it.
    pub record_copy_out_of_band: Option<bool>,
    pub durable_store_delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct CompositeSubStoreSettings {
    pub local: Option<LocalImmutableStoreSettings>,
    pub mode: String,
    pub remote: Option<RemoteStoreSettings>,
    pub replicated: Option<ReplicatedStoreSettings>,
    pub replication_mode: Option<ReplicationMode>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct ImmutableStoreSettings {
    pub composite: Option<CompositeStoreSettings>,
    pub local: Option<LocalImmutableStoreSettings>,
    pub mode: String,
    pub remote: Option<RemoteStoreSettings>,
    pub replicated: Option<ReplicatedStoreSettings>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct LocalImmutableStoreSettings {
    pub compaction_delay: Option<usize>,
    pub eviction_delay: Option<usize>,
    pub flush_delay_seconds: u16,
    /// Filesystem location for the local store. When empty (the default), the
    /// server derives `<system temp dir>/lore-server` at startup so a stand
    /// alone server binary with no external config can run as-is.
    #[serde(default)]
    pub path: String,
    pub max_capacity: Option<usize>,
    pub max_size: Option<usize>,
    pub target_capacity_percentage: Option<usize>,
    pub target_size_percentage: Option<usize>,
    pub compaction_parallel_groups: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct LocalMutableStoreSettings {
    pub flush_delay_seconds: u16,
    /// Filesystem location for the local store. When empty (the default), the
    /// server derives `<system temp dir>/lore-server` at startup so a stand
    /// alone server binary with no external config can run as-is.
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct MutableStoreSettings {
    pub local: Option<LocalMutableStoreSettings>,
    pub mode: String,
    pub remote: Option<RemoteStoreSettings>,
}

#[derive(Clone, Default, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct RemoteStoreSettings {
    pub auth_url: Option<String>,
    pub remote_url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReplicatedStoreSettings {
    pub remote_url: String,
    pub certs: Option<CertificateSettings>,
    pub regenerate_retry: RetrySettings,
    pub periodic_client_refresh_secs: u64,
    #[serde(default = "default_quic_client_monitor_interval_secs")]
    pub client_metrics_interval_seconds: u64,
    /// how many inflight messages are allowed before we self-throttle
    pub client_message_limit: Option<usize>,
    pub client_max_reconnects: Option<u32>,
    pub max_bandwidth_bytes_per_second: Option<u64>,
    pub expected_rtt_ms: Option<u64>,
}

#[derive(Copy, Clone, Default, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationMode {
    Read,
    Write,
    #[default]
    ReadWrite,
}

/// Lock-related settings
///
/// Settings for lock store configuration using dynamic plugin selection.
#[derive(Clone, Debug, Default, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct LockStoreSettings {
    /// The lock store plugin mode (e.g., "dynamodb", "local")
    #[allow(dead_code)]
    pub mode: String,
}

/// Notification system configuration.
///
/// The `mode` field selects the notification backend:
/// - `"local"` (default) - In-process broadcast channels, no external dependencies
/// - Any other value - Looked up as a notification plugin in the `PluginRegistry`
///
/// Plugin-specific configuration is provided through the top-level `[plugins.<name>]`
/// section in the config file, not in this struct.
#[derive(Clone, Debug, Default, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct NotificationSettings {
    /// The notification backend mode. Defaults to "local" if not specified.
    #[serde(default = "default_notification_mode")]
    pub mode: String,
}

fn default_notification_mode() -> String {
    "local".to_string()
}
