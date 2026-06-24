// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Server entry point for Lore Server.
//!
//! This module provides [`server_main()`], the public entry point that both the
//! base `loreserver` binary and derived server binaries (e.g., `lore-server-epic`)
//! call after configuring their [`ServerConfig`](crate::server_config::ServerConfig).

#[cfg(feature = "seeding")]
use std::any::Any;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::LoreTaskLifecycleEvent;
use lore_base::runtime::LoreTaskSpawnLocation;
use lore_base::runtime::runtime;
use lore_base::runtime::runtime_with_settings;
use lore_base::runtime::set_task_lifecycle_callback;
use lore_base::version::LORE_LIBRARY_VERSION;
use lore_revision::cluster::topology::Topology;
use lore_revision::environment::EnvironmentConfig;
use lore_revision::lock::LockStore;
use lore_revision::notification::NotificationSender;
use lore_revision::runtime::execution_context;
use lore_revision::store::composite::CompositeStoreBuilder;
use lore_revision::store::remote::RemoteImmutableStore;
use lore_revision::store::remote::RemoteMutableStore;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::assume_server_policies;
use lore_storage::compress::COMPRESSION_MODE;
use lore_storage::hash::StringHash;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_telemetry::execution_state::ServerExecutionState;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use lore_transport::grpc::set_user_agent;
use lore_transport::quic::client;
use lore_transport::quic::client::ClientCerts;
use lore_transport::quic::client::ServiceClient;
use lore_transport::quic::storage_service::client::StorageClient;
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::ResourceDetector;
use rustls::server::NoClientAuth;
use tokio::runtime::Handle;
use tokio::task::JoinSet;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::trace;
use tracing::warn;

use crate::auth::jwk::JwkServiceImpl;
use crate::auth::jwt::JwtVerifier;
use crate::grpc::GrpcInternalServerBuilder;
use crate::grpc::GrpcServerBuilder;
use crate::grpc::notification_service::NotificationService;
use crate::hooks::HookDispatcher;
use crate::hooks::HookRegistrationContext;
use crate::hooks::HookRegistry;
use crate::http::LoreHttpServer;
use crate::http::server::LoreHttpServerSettings;
use crate::http::server::PresignSettings;
use crate::plugins;
use crate::plugins::PluginRegistry;
use crate::plugins::traits::NotificationPluginContext;
use crate::protocol::attribute_map::AttributeMap;
use crate::quic::StreamDataHandler;
use crate::quic::StreamHandlerFactory;
use crate::quic::quinn::QuinnConfigBuilder;
use crate::quic::quinn::QuinnServer;
use crate::quic::quinn::build_cert_verifier;
use crate::quic::quinn::service_store::ServiceStore;
use crate::quic::quinn::service_store::StreamDataHandlerBuilder;
use crate::quic::replication_store_service;
use crate::quic::replication_store_service::client::ReplicationStoreClient;
use crate::quic::replication_store_service::client_container;
use crate::quic::replication_store_service::client_container::ClientContainerConfig;
use crate::quic::replication_store_service::server::ReplicationStoreService;
use crate::quic::storage_service::StorageService;
use crate::quic::stream_handler::StreamHandler;
use crate::server_config::ServerConfig;
use crate::settings::CompositeStoreSettings;
use crate::settings::CompositeSubStoreSettings;
use crate::settings::LocalImmutableStoreSettings;
use crate::settings::LocalMutableStoreSettings;
use crate::settings::NotificationSettings;
use crate::settings::QuicSettings;
use crate::settings::RemoteStoreSettings;
use crate::settings::ReplicatedStoreSettings;
use crate::settings::ReplicationMode;
use crate::settings::Settings;
use crate::store::replica_factory::ReplicationStoreTargetFactory;
use crate::store::replicated_store::ReplicatedStore;
use crate::store::resolve_plugin_config_with_fallback;
use crate::telemetry::OtelTokioRuntimeMetrics;
use crate::telemetry::ResourceDetectorProvider;
use crate::telemetry::TelemetryInitializer;
use crate::tls::load_client_tls;
use crate::topology::configure_topology_with_registry;
use crate::util::setup_execution;

/// Store mode constants for string-based configuration of built-in store types
mod store_mode {
    pub const LOCAL: &str = "local";
    pub const REMOTE: &str = "remote";
    pub const COMPOSITE: &str = "composite";
    pub const REPLICATED: &str = "replicated";
}

/// Command-line options for the Lore server binary.
///
/// All configuration is optional: with no arguments the server starts from the
/// defaults baked into the binary. Both flags also fall back to their
/// corresponding environment variables.
#[derive(Debug, Parser)]
#[command(name = "loreserver", version, about = "Lore revision control server")]
pub struct Cli {
    /// Directory of TOML config files layered over the built-in defaults.
    ///
    /// When set, the server loads (if present) `default.toml`,
    /// `<environment>.toml`, `<environment>_<region>.toml`, and `local.toml`
    /// from this directory as overrides. When unset, only the built-in defaults
    /// and environment variables are used.
    #[arg(long, value_name = "DIR", env = "LORE_CONFIG_PATH")]
    pub config: Option<String>,

    /// Environment name selecting the `<environment>.toml` override to load.
    ///
    /// Defaults to `local` when neither the flag nor `LORE_ENV` is set.
    #[arg(long, value_name = "ENV", env = "LORE_ENV")]
    pub env: Option<String>,
}

/// Entry point for the Lore server.
///
/// Both the base `loreserver` binary and derived server binaries call this
/// function after configuring their [`ServerConfig`] with plugins, hooks,
/// and resource detectors.
///
/// This function:
/// 1. Loads settings from the configuration files
/// 2. Creates and configures the tokio runtime
/// 3. Initializes telemetry with the provided resource detectors
/// 4. Registers plugins and hooks
/// 5. Configures stores, starts endpoints, and runs until shutdown
///
/// # Example
///
/// ```no_run
/// use lore_server::server_config::ServerConfig;
///
/// // Base server with no plugins
/// lore_server::server::server_main(ServerConfig::default()).unwrap();
/// ```
pub fn server_main(config: ServerConfig) -> Result<()> {
    set_user_agent(format!("lore-server/{}", LORE_LIBRARY_VERSION.as_str()));
    assume_server_policies();

    let cli = Cli::parse();
    let (settings, settings_hash) = Settings::load(cli.config.as_deref(), cli.env.as_deref())?;
    let runtime_shutdown_timeout = settings.server.runtime_shutdown_timeout_seconds;

    lore_base::log::set_log_callback(Some(server_log_dispatch));

    let result = match settings.tokio.as_ref() {
        Some(tokio) => runtime_with_settings(Some(tokio.clone())),
        None => runtime(),
    }
    .block_on({
        let execution = setup_execution(module_path!(), String::default(), String::default());
        #[allow(clippy::large_futures)]
        LORE_CONTEXT.scope(execution, async move {
            async_main((settings, settings_hash), config).await
        })
    });

    info!("Wait up to {runtime_shutdown_timeout} seconds for runtime shutdown");
    lore_base::runtime::runtime_shutdown_timeout(Duration::from_secs(
        runtime_shutdown_timeout as u64,
    ));

    result
}

/// Waits for a shutdown signal, then drains all endpoints within the given timeout.
///
/// 1. Waits for the shutdown signal (watch channel becomes `true`) or for an
///    endpoint to exit unexpectedly (which triggers the signal).
/// 2. Records the time the signal was received.
/// 3. Joins all remaining endpoint tasks.
/// 4. If the elapsed time since the signal exceeds `connection_close_timeout`,
///    force-closes any remaining endpoints.
async fn wait_for_shutdown(
    mut endpoints: JoinSet<Result<()>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    connection_close_timeout: Duration,
) -> Result<()> {
    let mut shutdown_rx = shutdown_tx.subscribe();

    // Phase 1: wait for signal or unexpected endpoint exit
    info!("Server is up, waiting for shutdown signal");
    tokio::select! {
        _ = shutdown_rx.wait_for(|&v| v) => {}
        res = endpoints.join_next() => {
            match res {
                Some(Ok(Err(e))) => {
                    error!("Endpoint returned error: {e:?}, triggering shutdown");
                    let _ = shutdown_tx.send(true);
                }
                Some(Err(e)) => {
                    error!("Endpoint task failed: {e}, triggering shutdown");
                    let _ = shutdown_tx.send(true);
                }
                Some(Ok(Ok(()))) => {
                    info!("Endpoint exited, triggering shutdown");
                    let _ = shutdown_tx.send(true);
                }
                None => {
                    info!("All endpoints completed");
                    return Ok(());
                }
            }
        }
    }

    // Phase 2: drain remaining endpoints, timeout measured from now
    let deadline = tokio::time::Instant::now() + connection_close_timeout;
    info!("Draining remaining endpoints (timeout: {connection_close_timeout:?})");

    loop {
        tokio::select! {
            res = endpoints.join_next() => {
                match res {
                    Some(Ok(Ok(()))) => info!("Endpoint shut down successfully"),
                    Some(Ok(Err(e))) => error!("Endpoint returned error during shutdown: {e:?}"),
                    Some(Err(e)) => error!("Endpoint task failed: {e}"),
                    None => {
                        info!("All endpoints shut down gracefully");
                        return Ok(());
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                warn!(
                    "Connection close timeout ({connection_close_timeout:?}) exceeded, \
                     force-closing remaining endpoints"
                );
                endpoints.shutdown().await;
                return Ok(());
            }
        }
    }
}

async fn listen_for_termination(shutdown_tx: tokio::sync::watch::Sender<bool>) -> Result<()> {
    #[cfg(unix)]
    let (mut ctrl_c, mut sigterm) = {
        use tokio::signal::unix::SignalKind;
        use tokio::signal::unix::signal;
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
        )
    };

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c.recv() => {
            info!("Received SIGINT. Initiating graceful shutdown");
        },
        _ = sigterm.recv() => {
            info!("Received SIGTERM. Initiating graceful shutdown");
        }
    }

    #[cfg(windows)]
    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            info!("Received CTRL-C. Initiating graceful shutdown");
        }
        Err(err) => {
            eprintln!("Unable to listen for shutdown signal: {err}");
        }
    }

    let _ = shutdown_tx.send(true);
    Ok(())
}

impl From<QuicSettings> for QuinnConfigBuilder {
    fn from(settings: QuicSettings) -> Self {
        let mut builder = QuinnConfigBuilder::default().num_listeners(settings.num_listeners);

        if let Some(v) = settings.idle_timeout {
            builder = builder.idle_timeout(Duration::from_millis(v));
        };

        if let Some(v) = settings.keep_alive {
            builder = builder.keep_alive(Duration::from_millis(v));
        };

        if let Some(v) = settings.max_bidi_streams {
            builder = builder.max_bidi_streams(v);
        };

        if let Some(v) = settings.transport_bits_per_second {
            builder = builder.transport_bits_per_second(v);
        }

        if let Some(v) = settings.transport_rtt {
            builder = builder.transport_rtt(v);
        }

        builder
    }
}

async fn launch_quinn_server(
    name: &'static str,
    stream_handler_factory: Box<dyn StreamHandlerFactory>,
    metrics_frequency: Duration,
    quic_settings: QuicSettings,
    generate_ephemeral_cert: bool,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let span = info_span!("QUIC server", name);

    async {
        let cert_settings = match quic_settings.certificate.clone() {
            Some(cert_settings) => cert_settings,
            None if generate_ephemeral_cert => generate_ephemeral_certificate(name)?,
            None => return Err(anyhow!("Missing QUIC certificate config")),
        };

        let client_verifier = if quic_settings.verify_client_certs {
            let ca_path = cert_settings
                .cert_chain
                .clone()
                .ok_or(anyhow!("Missing cert chain"))?;
            build_cert_verifier(ca_path)?
        } else {
            Arc::new(NoClientAuth {})
        };

        let addr = SocketAddr::from_str(
            format!("{}:{}", quic_settings.host, quic_settings.port).as_str(),
        )?;

        let mut settings_builder: QuinnConfigBuilder = quic_settings.into();

        settings_builder = settings_builder
            .server_metrics_name(name)
            .address(addr)
            .cert_chain(cert_settings.cert_chain)
            .cert_file(cert_settings.cert_file)
            .pkey_file(cert_settings.pkey_file)
            .client_cert_verifier(client_verifier)
            .stream_handler_factory(stream_handler_factory)
            .metrics_frequency(metrics_frequency);

        info!(address = %addr, "server starting");
        let server = QuinnServer::start(settings_builder.build()?)?;

        // Wait for the shutdown signal, then close the endpoint gracefully.
        // close() sends CONNECTION_CLOSE frames to all peers and causes
        // accept() loops to return None.
        let _ = shutdown_rx.wait_for(|&v| v).await;
        info!("closing endpoint");
        server.close().await;

        Ok(())
    }
    .instrument(span)
    .await
}

/// Returns the names of the optional cargo features this server binary was
/// compiled with. Reported through the `ServerInfo` RPC so clients and tests
/// can detect capabilities that are only present in some builds (for example
/// `failure_generator`, which enables fault-injection used by smoke tests).
fn compiled_features() -> Vec<String> {
    let mut features = Vec::new();
    if cfg!(feature = "failure_generator") {
        features.push("failure_generator".to_string());
    }
    if cfg!(feature = "oodle") {
        features.push("oodle".to_string());
    }
    if cfg!(feature = "seeding") {
        features.push("seeding".to_string());
    }
    features
}

#[allow(clippy::too_many_arguments)]
async fn launch_grpc_server(
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
    jwt_verifier: Option<JwtVerifier>,
    settings: Settings,
    notification_sender: Arc<dyn NotificationSender>,
    notification_service: Option<NotificationService>,
    hook_dispatcher: Arc<HookDispatcher>,
    user_agent_filter: Arc<UserAgentFilter>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let grpc_settings = settings
        .server
        .grpc
        .clone()
        .ok_or(anyhow!("Missing gRPC settings"))?;
    let service_settings = settings.server.grpc_public_services.clone();

    let addr =
        SocketAddr::from_str(format!("{}:{}", grpc_settings.host, grpc_settings.port).as_str())?;

    info!(
        "Starting Lore GRPC Server: {}, Auth: {} Locks: {}",
        &addr,
        jwt_verifier.as_ref().map_or("disabled", |_| "enabled"),
        lock_store.as_ref().map_or("disabled", |_| "enabled"),
    );

    // The settings map has no relevant entries to surface yet, so it stays empty.
    // The features list reports the cargo features the binary was compiled with so
    // clients and tests can detect optional capabilities (e.g. failure_generator).
    let settings_map: HashMap<String, String> = HashMap::new();
    let features_list = compiled_features();

    let (cert_path, key_path, cert_chain_path) =
        if let Some(cert_settings) = grpc_settings.certificate {
            (
                Some(cert_settings.cert_file),
                Some(cert_settings.pkey_file),
                cert_settings.cert_chain,
            )
        } else {
            (None, None, None)
        };

    let mut environment = settings.environment.clone().unwrap_or_default();
    let feature = settings.feature.clone().unwrap_or_default();

    // Enforce store limits
    if let Some(limit) = immutable_store.max_query_batch() {
        let mut config = environment.config.unwrap_or_default();
        let previous_limit = config.max_query_batch.unwrap_or_default();
        config.max_query_batch = Some(if previous_limit == 0 || limit < previous_limit {
            limit
        } else {
            previous_limit
        });
        environment.config = Some(config);
    }

    GrpcServerBuilder::new()
        .with_environment(environment)
        .with_feature(feature)
        .with_immutable_store(immutable_store, local_store)
        .with_mutable_store(mutable_store)
        .with_lock_store(lock_store)
        .with_notification(notification_sender, notification_service)
        .with_hook_dispatcher(hook_dispatcher)
        .with_tls_config(cert_path, key_path, cert_chain_path)?
        .with_admin_endpoints(settings_map, features_list)
        .with_http2_config(
            grpc_settings
                .http2_keepalive_interval_seconds
                .map(Duration::from_secs),
            grpc_settings
                .http2_keepalive_timeout_seconds
                .map(Duration::from_secs),
            Duration::from_secs(grpc_settings.request_handler_timeout_seconds),
            service_settings,
            user_agent_filter,
        )
        .with_jwt_verifier(jwt_verifier)?
        .serve(addr, async move {
            let _ = shutdown_rx.wait_for(|&v| v).await;
        })
        .await
}

/// Outcome of [`validate_endpoint_security`]: either the endpoint is
/// locked down with mTLS, or it is starting unauthenticated and the
/// caller must surface a warning at startup.
#[derive(Debug, PartialEq, Eq)]
enum EndpointSecurity {
    Mtls,
    Untrusted,
}

/// Decide whether a server-to-server endpoint may start, and on what
/// terms. Used by the gRPC internal server and the internal QUIC
/// listener — both grant blanket access to the storage layer and rely
/// solely on mTLS for authentication. The only way to start without
/// mTLS is the explicit `verify_client_certs = false` opt-in.
///
/// `label` is the config-section path used in error messages
/// (e.g. `"[server.grpc_internal]"` or `"[server.quic_internal]"`).
///
/// Returns:
/// - `Ok(Mtls)` when `verify_client_certs = true` and `certificate`
///   carries a full triple (`cert_file` + `pkey_file` + `cert_chain`).
/// - `Ok(Untrusted)` when `verify_client_certs = false`. The caller is
///   responsible for emitting a startup warning.
/// - `Err` when `verify_client_certs = true` but the certificate is
///   missing or only partially configured.
fn validate_endpoint_security(
    label: &str,
    certificate: Option<&crate::tls::CertificateSettings>,
    verify_client_certs: bool,
) -> Result<EndpointSecurity> {
    if !verify_client_certs {
        return Ok(EndpointSecurity::Untrusted);
    }
    match certificate {
        Some(cert) if cert.cert_chain.is_some() => Ok(EndpointSecurity::Mtls),
        Some(_) => Err(anyhow!(
            "{label} certificate is partially configured: \
             certificate.cert_file and certificate.pkey_file are set but \
             certificate.cert_chain (the CA used to verify client certs) \
             is missing. The endpoint requires mTLS, not server-only TLS"
        )),
        None => Err(anyhow!(
            "{label} requires mTLS to start (verify_client_certs = true). \
             Configure certificate.cert_file, certificate.pkey_file, and \
             certificate.cert_chain, or set verify_client_certs = false \
             to explicitly accept the security exposure"
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn launch_grpc_internal_server(
    settings: Settings,
    user_agent_filter: Arc<UserAgentFilter>,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: Arc<HookDispatcher>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let grpc_settings = settings
        .server
        .grpc_internal
        .ok_or(anyhow!("Missing gRPC internal settings"))?;

    let addr =
        SocketAddr::from_str(format!("{}:{}", grpc_settings.host, grpc_settings.port).as_str())?;

    info!("Starting Lore gRPC internal server: {}", &addr);

    let (cert_path, key_path, cert_chain_path) =
        if let Some(cert_settings) = grpc_settings.certificate {
            (
                Some(cert_settings.cert_file),
                Some(cert_settings.pkey_file),
                cert_settings.cert_chain,
            )
        } else {
            (None, None, None)
        };

    let rpc_timeout = Duration::from_secs(grpc_settings.request_handler_timeout_seconds);

    GrpcInternalServerBuilder::new()
        .with_components(
            local_store().ok_or(anyhow!(
                "Cannot configure gRPC internal server, no local store"
            ))?,
            immutable_store,
            mutable_store,
            notification_sender,
            hook_dispatcher,
        )?
        .with_tls_config(cert_path, key_path, cert_chain_path)?
        .with_http2_config(
            grpc_settings
                .http2_keepalive_interval_seconds
                .map(Duration::from_secs),
            grpc_settings
                .http2_keepalive_timeout_seconds
                .map(Duration::from_secs),
            user_agent_filter,
            rpc_timeout,
        )?
        .serve(addr, async move {
            let _ = shutdown_rx.wait_for(|&v| v).await;
        })
        .await
}

async fn launch_http_server(
    settings: LoreHttpServerSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    jwt_verifier: Option<JwtVerifier>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    LoreHttpServer::serve(
        settings,
        immutable_store,
        mutable_store,
        jwt_verifier,
        async move {
            let _ = shutdown_rx.wait_for(|&v| v).await;
        },
    )
    .await
}

async fn launch_maintenance_grpc_server(
    settings: Settings,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let grpc_settings = settings
        .server
        .grpc
        .clone()
        .ok_or(anyhow!("Missing gRPC settings"))?;

    let addr =
        SocketAddr::from_str(format!("{}:{}", grpc_settings.host, grpc_settings.port).as_str())?;

    info!("Starting Lore maintenance gRPC Server: {}", &addr);

    let environment = settings.environment.clone().unwrap_or_default();

    let (cert_path, key_path, cert_chain_path) =
        if let Some(cert_settings) = grpc_settings.certificate {
            (
                Some(cert_settings.cert_file),
                Some(cert_settings.pkey_file),
                cert_settings.cert_chain,
            )
        } else {
            (None, None, None)
        };

    crate::grpc::server::serve_maintenance(
        environment,
        addr,
        cert_path,
        key_path,
        cert_chain_path,
        async move {
            let _ = shutdown_rx.wait_for(|&v| v).await;
        },
    )
    .await
}

struct QuicPublicStreamHandler {
    service_store: ServiceStore,
}

impl QuicPublicStreamHandler {
    fn new(
        immutable_store: Arc<dyn ImmutableStore>,
        local_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
        jwt_verifier: Option<JwtVerifier>,
        process_limit: usize,
        handler_duration_timeout: Option<Duration>,
    ) -> Self {
        let mut service_store = ServiceStore::default();

        let make_storage_handler =
            |immutable_store: Arc<dyn ImmutableStore>,
             local_store: Arc<dyn ImmutableStore>,
             mutable_store: Arc<dyn MutableStore>,
             jwt_verifier: Option<JwtVerifier>| {
                Box::new(move |context: Arc<AttributeMap>| {
                    let storage_protocol = StorageService::new(
                        Arc::new(jwt_verifier.clone()),
                        immutable_store.clone(),
                        local_store.clone(),
                        mutable_store.clone(),
                    );
                    Box::new(StreamHandler::new(
                        Arc::new(storage_protocol),
                        context,
                        process_limit,
                        handler_duration_timeout,
                    )) as Box<dyn StreamDataHandler>
                }) as StreamDataHandlerBuilder
            };

        service_store.add_service(
            "urc/0.2",
            make_storage_handler(
                immutable_store.clone(),
                local_store.clone(),
                mutable_store.clone(),
                jwt_verifier.clone(),
            ),
        );
        {
            let immutable_store = immutable_store.clone();
            let local_store = local_store.clone();
            let mutable_store = mutable_store.clone();
            let jwt_verifier = jwt_verifier.clone();
            service_store.add_service(
                StorageClient::ALPN,
                Box::new(move |context: Arc<AttributeMap>| {
                    let v4_service = crate::quic::storage_service_v4::StorageServiceV4::new(
                        Arc::new(jwt_verifier.clone()),
                        immutable_store.clone(),
                        local_store.clone(),
                        mutable_store.clone(),
                    );
                    Box::new(StreamHandler::new(
                        Arc::new(v4_service),
                        context,
                        process_limit,
                        handler_duration_timeout,
                    )) as Box<dyn StreamDataHandler>
                }) as StreamDataHandlerBuilder,
            );
        }

        Self { service_store }
    }
}

impl StreamHandlerFactory for QuicPublicStreamHandler {
    fn supported_protocols(&self) -> Vec<String> {
        self.service_store.get_supported_services()
    }

    fn get_stream_handler_builder(
        &self,
        protocol: &str,
    ) -> Option<(&&'static str, &StreamDataHandlerBuilder)> {
        self.service_store.get_stream_builder(protocol)
    }
    fn name(&self) -> &'static str {
        "QuicPublicStreamHandler"
    }
}

struct QuicInternalStreamHandler {
    service_store: ServiceStore,
}

impl QuicInternalStreamHandler {
    fn new(
        immutable_store: Arc<dyn ImmutableStore>,
        local_store: Arc<dyn ImmutableStore>,
        process_limit: usize,
        handler_duration_timeout: Option<Duration>,
    ) -> Self {
        let mut service_store = ServiceStore::default();
        {
            service_store.add_service(
                ReplicationStoreClient::ALPN,
                Box::new(move |context: Arc<AttributeMap>| {
                    let protocol =
                        ReplicationStoreService::new(immutable_store.clone(), local_store.clone());
                    Box::new(StreamHandler::new(
                        Arc::new(protocol),
                        context,
                        process_limit,
                        handler_duration_timeout,
                    ))
                }),
            );
        }

        Self { service_store }
    }
}

impl StreamHandlerFactory for QuicInternalStreamHandler {
    fn supported_protocols(&self) -> Vec<String> {
        self.service_store.get_supported_services()
    }

    fn get_stream_handler_builder(
        &self,
        protocol: &str,
    ) -> Option<(&&'static str, &StreamDataHandlerBuilder)> {
        self.service_store.get_stream_builder(protocol)
    }
    fn name(&self) -> &'static str {
        "QuicInternalStreamHandler"
    }
}

async fn configure_immutable_store_via_plugin(
    registry: &PluginRegistry,
    settings: &Settings,
    topology: Option<Arc<dyn Topology + Send + Sync>>,
) -> Result<Arc<dyn ImmutableStore>> {
    let mode = &settings.immutable_store.mode;

    match mode.as_str() {
        store_mode::LOCAL => {
            let local_settings = settings
                .immutable_store
                .local
                .as_ref()
                .ok_or(anyhow!("Missing local immutable store settings"))?;

            configure_local_immutable_store(local_settings).await
        }
        store_mode::COMPOSITE => {
            let composite_settings = settings
                .immutable_store
                .composite
                .as_ref()
                .ok_or(anyhow!("Missing composite store settings"))?;

            configure_composite_store(registry, composite_settings, settings, topology).await
        }
        store_mode::REPLICATED => {
            let replicated_settings = settings
                .immutable_store
                .replicated
                .as_ref()
                .ok_or(anyhow!("Missing replicated store settings"))?;

            configure_replicated_immutable_store(replicated_settings).await
        }
        store_mode::REMOTE => {
            let remote_settings = settings
                .immutable_store
                .remote
                .as_ref()
                .ok_or(anyhow!("Missing remote immutable store settings"))?;

            configure_remote_immutable_store(remote_settings)
        }
        _ => {
            // All other modes use the plugin system
            let plugin_config =
                resolve_plugin_config_with_fallback(&settings.plugins, mode, "immutable_store")
                    .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));

            info!(mode, "Creating immutable store via plugin system");

            registry
                .create_immutable_store(mode, &plugin_config)
                .map_err(|e| anyhow!("Failed to create immutable store plugin '{mode}': {e}"))
        }
    }
}

async fn configure_mutable_store_via_plugin(
    registry: &PluginRegistry,
    settings: &Settings,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<Arc<dyn MutableStore>> {
    let mode = &settings.mutable_store.mode;

    match mode.as_str() {
        store_mode::LOCAL => {
            let local_settings = settings
                .mutable_store
                .local
                .as_ref()
                .ok_or(anyhow!("Missing local mutable store settings"))?;

            configure_local_mutable_store(local_settings, immutable_store).await
        }
        store_mode::REMOTE => {
            let remote_settings = settings
                .mutable_store
                .remote
                .as_ref()
                .ok_or(anyhow!("Missing remote mutable store settings"))?;

            configure_remote_mutable_store(remote_settings)
        }
        store_mode::REPLICATED => Err(anyhow!("replicated mutable store is not implemented")),
        store_mode::COMPOSITE => Err(anyhow!(
            "Invalid settings, cannot have composite store as mutable store"
        )),
        _ => {
            // All other modes use the plugin system
            let plugin_config =
                resolve_plugin_config_with_fallback(&settings.plugins, mode, "mutable_store")
                    .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));

            info!(mode, "Creating mutable store via plugin system");

            registry
                .create_mutable_store(mode, &plugin_config, immutable_store)
                .map_err(|e| anyhow!("Failed to create mutable store plugin '{mode}': {e}"))
        }
    }
}

fn configure_lock_store_via_plugin(
    registry: &PluginRegistry,
    settings: &Settings,
) -> Result<Option<Arc<dyn LockStore>>> {
    if let Some(lock_settings) = &settings.lock_store {
        let mode = &lock_settings.mode;

        if mode == store_mode::LOCAL {
            info!("Creating local (in-memory) lock store");
            let store = crate::lock::store::LocalLockStore::default();
            return Ok(Some(Arc::new(store)));
        }

        // All other modes use the plugin system
        let plugin_config =
            resolve_plugin_config_with_fallback(&settings.plugins, mode, "lock_store")
                .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));

        info!(mode, "Creating lock store via plugin system");

        let store = registry
            .create_lock_store(mode, &plugin_config)
            .map_err(|e| anyhow!("Failed to create lock store plugin '{mode}': {e}"))?;

        return Ok(Some(store));
    }

    Ok(None)
}

static LOCAL_STORE: OnceLock<Weak<dyn ImmutableStore>> = OnceLock::new();

fn local_store() -> Option<Arc<dyn ImmutableStore>> {
    LOCAL_STORE.get().and_then(|weak| weak.upgrade())
}

/// Directory under the system temporary directory where the server keeps
/// zero-config artifacts (local stores and ephemeral certificates) when no
/// explicit locations are configured.
fn local_data_dir() -> PathBuf {
    std::env::temp_dir().join("lore-server")
}

/// Resolve the configured local store path, falling back to a directory under
/// the system temporary directory when none was provided.
///
/// The resolved path is logged so operators can see where on-disk state lives.
/// When no path was configured, the generated temporary path is logged as a
/// prominent warning because that location is ephemeral and not persisted
/// across reboots.
fn resolve_local_store_path(configured: &str, store_label: &str) -> PathBuf {
    if configured.trim().is_empty() {
        let path = local_data_dir();
        warn!(
            store = store_label,
            path = %path.display(),
            "No local store path configured for the '{}' store; generated a path under the system \
             temporary directory: {}. This data is EPHEMERAL and not persisted across reboots — \
             configure an explicit path for production.",
            store_label,
            path.display(),
        );
        path
    } else {
        let path = PathBuf::from(configured);
        info!(store = store_label, path = %path.display(), "Using configured local store path");
        path
    }
}

/// Build a [`CertificateSettings`](crate::tls::CertificateSettings) for an
/// endpoint that has no certificate configured by generating an ephemeral
/// self-signed certificate and writing it under the system temporary directory.
///
/// Used for the user-facing QUIC endpoint so a stand alone server binary with
/// no external config can serve TLS out of the box. A prominent warning is
/// logged because these certificates are untrusted and regenerated on every
/// startup.
fn generate_ephemeral_certificate(endpoint: &str) -> Result<crate::tls::CertificateSettings> {
    let dir = local_data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        anyhow!(
            "failed to create directory {} for ephemeral certificate: {e}",
            dir.display()
        )
    })?;

    let cert_file = dir.join(format!("{endpoint}-cert.pem"));
    let pkey_file = dir.join(format!("{endpoint}-key.pem"));

    let generated = lore_transport::tls::generate_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])?;

    std::fs::write(&cert_file, generated.cert_pem).map_err(|e| {
        anyhow!(
            "failed to write ephemeral certificate {}: {e}",
            cert_file.display()
        )
    })?;
    std::fs::write(&pkey_file, generated.key_pem).map_err(|e| {
        anyhow!(
            "failed to write ephemeral private key {}: {e}",
            pkey_file.display()
        )
    })?;

    warn!(
        endpoint,
        cert = %cert_file.display(),
        key = %pkey_file.display(),
        "No TLS certificate configured for the '{endpoint}' QUIC endpoint; generated an \
         EPHEMERAL SELF-SIGNED certificate. This is untrusted, regenerated on every restart, \
         and intended for local development only. Configure a real certificate for production."
    );

    Ok(crate::tls::CertificateSettings {
        cert_chain: None,
        cert_file,
        pkey_file,
    })
}

async fn create_local_store(
    settings: &LocalImmutableStoreSettings,
    flush_background: bool,
) -> Result<Arc<dyn ImmutableStore>> {
    let default_settings = lore_storage::local::immutable_store::ImmutableStoreSettings::default();
    let store_path = resolve_local_store_path(&settings.path, "immutable");

    let options = ImmutableStoreCreateOptions {
        max_capacity: settings.max_capacity,
        eviction_delay: settings
            .eviction_delay
            .map(|ms| std::time::Duration::from_millis(ms as u64)),
        max_size: settings.max_size,
        compaction_delay: settings
            .compaction_delay
            .map(|ms| std::time::Duration::from_millis(ms as u64)),
    };

    let store = lore_storage::local::immutable_store::create(
        Some(store_path.as_path()),
        options,
        true,  /* Server mode, deserialize all buckets immediately */
        lore_storage::local::immutable_store::ImmutableStoreSettings {
            allow_partial_fragment: false, /* Server mode, partial fragments not allowed */
            protect_local_fragment: false, /* Server mode, no need to try protect local fragments from eviction */
            implicit_durable_stored: true, /* Server mode, consider all fragments as durably stored */
            flush_background,
            flush_delay_seconds: settings.flush_delay_seconds as u64,
            target_capacity_percentage: settings.target_capacity_percentage.unwrap_or(default_settings.target_capacity_percentage),
            target_size_percentage: settings.target_size_percentage.unwrap_or(default_settings.target_size_percentage),
            compaction_parallel_groups: settings.compaction_parallel_groups.unwrap_or(default_settings.compaction_parallel_groups),
            verify_write: false,
            atime: false,
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX, /* Server mode, full 256-bucket layout from the start */
            fan_out_threshold: lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
        },
    )
    .await
    .map_err(anyhow::Error::from)?;

    lore_storage::maintenance::spawn_gc(&store, &options);

    Ok(store)
}

async fn configure_local_immutable_store(
    settings: &LocalImmutableStoreSettings,
) -> Result<Arc<dyn ImmutableStore>> {
    if let Some(local_store) = local_store() {
        return Ok(local_store.clone());
    }

    info!("Wiring up local immutable store");

    let store = create_local_store(settings, true /* flush background */).await?;

    let weak_store = Arc::downgrade(&store);

    // Only time this returns an error is if the value is already set.
    let _ = LOCAL_STORE.set(weak_store.clone());

    Ok(store)
}

async fn configure_local_mutable_store(
    settings: &LocalMutableStoreSettings,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<Arc<dyn MutableStore>> {
    info!("Wiring up local mutable store");

    let store_path = resolve_local_store_path(&settings.path, "mutable");

    Ok(lore_storage::local::mutable_store::create(
        Some(store_path.as_path()),
        lore_storage::MutableStoreSettings {
            flush_delay_seconds: settings.flush_delay_seconds as u64,
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX, /* Server mode, full 256-bucket layout from the start */
            fan_out_threshold: lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
        },
        immutable_store,
    )
    .await?)
}

fn configure_remote_immutable_store(
    settings: &RemoteStoreSettings,
) -> Result<Arc<dyn ImmutableStore>> {
    info!(
        "Wiring up remote immutable store to {}",
        settings.remote_url
    );

    Ok(Arc::new(RemoteImmutableStore::new(
        settings.remote_url.as_str(),
        settings.auth_url.as_deref(),
    )))
}

async fn configure_replicated_immutable_store(
    settings: &ReplicatedStoreSettings,
) -> Result<Arc<dyn ImmutableStore>> {
    info!(
        "Wiring up replicated immutable store to {}",
        settings.remote_url
    );

    let certs = if let Some(setting_certs) = &settings.certs {
        let setting_certs = setting_certs.clone();
        client::CertificateSettings {
            custom_ca: setting_certs.cert_chain,
            client: Some(ClientCerts {
                cert_file: setting_certs.cert_file,
                pkey_file: setting_certs.pkey_file,
            }),
        }
    } else {
        client::CertificateSettings {
            custom_ca: None,
            client: None,
        }
    };

    let mut factory = client_container::QuicClientFactory::new(settings.remote_url.clone(), certs);
    if let Some(client_max_reconnects) = settings.client_max_reconnects {
        factory.quic_max_reconnects = Some(client_max_reconnects);
    } else {
        factory.quic_max_reconnects = Some(5);
    }
    if let Some(client_message_limit) = settings.client_message_limit {
        factory.command_behavior.message_limit = client_message_limit;
    }
    if let Some(max_bandwidth_bytes_per_second) = settings.max_bandwidth_bytes_per_second {
        factory.transport_config.max_bytes_bandwidth_per_second = max_bandwidth_bytes_per_second;
    }
    if let Some(expected_rtt_ms) = settings.expected_rtt_ms {
        factory.transport_config.expected_rtt_ms = expected_rtt_ms;
    }

    let container_config = ClientContainerConfig {
        regenerate_retry_policy: (&settings.regenerate_retry).into(),
        connection_lost_sleep: Duration::from_secs(1),
    };

    let store = ReplicatedStore::new(
        Arc::new(factory),
        container_config,
        Duration::from_secs(settings.periodic_client_refresh_secs),
        Duration::from_secs(settings.client_metrics_interval_seconds),
    )
    .await?;
    Ok(store)
}

fn configure_remote_mutable_store(settings: &RemoteStoreSettings) -> Result<Arc<dyn MutableStore>> {
    info!("Wiring up remote mutable store to {}", settings.remote_url);

    // TODO(mjansson): Identity handling
    let identity = None::<&str>;

    Ok(Arc::new(RemoteMutableStore::new(
        settings.remote_url.as_str(),
        identity,
    )))
}

async fn configure_composite_store(
    registry: &PluginRegistry,
    settings: &CompositeStoreSettings,
    global_settings: &Settings,
    topology: Option<Arc<dyn Topology + Send + Sync>>,
) -> Result<Arc<dyn ImmutableStore>> {
    info!("Wiring up Composite store");

    let mut composite_store_builder = CompositeStoreBuilder::default()
        .with_cache_query_results(settings.should_cache_query_results.unwrap_or_default());

    let store = Box::pin(configure_composite_substore(
        registry,
        &settings.local.mode,
        global_settings,
        &settings.local,
    ))
    .await?;
    composite_store_builder =
        composite_store_builder.with_local(settings.local.mode.clone(), store)?;

    if let Some(durable_settings) = settings.durable.as_ref() {
        let store = Box::pin(configure_composite_substore(
            registry,
            &durable_settings.mode,
            global_settings,
            durable_settings,
        ))
        .await?;
        composite_store_builder =
            composite_store_builder.with_durable(durable_settings.mode.clone(), store)?;
    }

    if let Some(replicas) = settings.replica.as_ref() {
        for store_settings in replicas.as_slice() {
            let store = Box::pin(configure_composite_substore(
                registry,
                &store_settings.mode,
                global_settings,
                store_settings,
            ))
            .await?;

            let read =
                store_settings.replication_mode.unwrap_or_default() != ReplicationMode::Write;
            let write =
                store_settings.replication_mode.unwrap_or_default() != ReplicationMode::Read;

            composite_store_builder = composite_store_builder.with_replica(
                store_settings.mode.clone(),
                store,
                read,
                write,
            );
        }
    }

    if let Some(factory_settings) = &settings.replica_factory {
        let (sni_override, quic_certs, grpc_tls) = if let Some(tls_settings) = &factory_settings.tls
        {
            info!(?tls_settings, "Loading Replica Client TLS certs");
            let quic_certs = client::CertificateSettings {
                custom_ca: tls_settings.client_certs.cert_chain.clone(),
                client: Some(ClientCerts {
                    cert_file: tls_settings.client_certs.cert_file.clone(),
                    pkey_file: tls_settings.client_certs.pkey_file.clone(),
                }),
            };
            let mut grpc_tls = load_client_tls(tls_settings.client_certs.clone())?;
            if let Some(server_sni) = &tls_settings.server_sni {
                grpc_tls = grpc_tls.domain_name(server_sni.clone());
            }
            (tls_settings.server_sni.clone(), quic_certs, Some(grpc_tls))
        } else {
            info!("No Replica client TLS defined");
            let quic_certs = client::CertificateSettings {
                custom_ca: None,
                client: None,
            };
            (None, quic_certs, None)
        };
        let mut factory = ReplicationStoreTargetFactory::new(
            grpc_tls,
            quic_certs,
            sni_override,
            factory_settings.client_message_buffer,
            factory_settings.read_replicas_enabled,
            factory_settings.use_grpc_write_replication,
        );
        factory.quic_monitor_interval =
            Duration::from_secs(factory_settings.quic_client_monitor_interval_seconds);
        factory.enable_same_region_write = factory_settings.enable_same_region_write;

        composite_store_builder = composite_store_builder.with_replica_builder(Arc::new(factory));
    }

    let store = Arc::new(composite_store_builder.build()?);

    if let Some(topology) = &topology {
        store
            .clone()
            .set_topology_subscription(topology.clone())
            .await;
    }

    Ok(store)
}

async fn configure_composite_substore(
    registry: &PluginRegistry,
    mode: &str,
    global_settings: &Settings,
    settings: &CompositeSubStoreSettings,
) -> Result<Arc<dyn ImmutableStore>> {
    match mode {
        store_mode::LOCAL => {
            let local_settings = settings
                .local
                .as_ref()
                .ok_or(anyhow!("Missing composite local store settings"))?;

            configure_local_immutable_store(local_settings).await
        }
        store_mode::REMOTE => {
            let remote_settings = settings
                .remote
                .as_ref()
                .ok_or(anyhow!("Missing composite remote store settings"))?;

            configure_remote_immutable_store(remote_settings)
        }
        store_mode::COMPOSITE => Err(anyhow!(
            "Composite store not supported as a composite substore"
        )),
        store_mode::REPLICATED => {
            let replicated_settings = settings
                .replicated
                .as_ref()
                .ok_or(anyhow!("Missing composite replicated store settings"))?;

            configure_replicated_immutable_store(replicated_settings).await
        }
        _ => {
            // All other modes use the plugin system
            let plugin_config = resolve_plugin_config_with_fallback(
                &global_settings.plugins,
                mode,
                "immutable_store",
            )
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));

            info!(mode, "Creating composite substore via plugin system");

            let store = registry
                .create_immutable_store(mode, &plugin_config)
                .map_err(|e| anyhow!("Failed to create {mode} immutable store: {e}"))?;

            Ok(store)
        }
    }
}

async fn configure_notification(
    endpoints: &mut JoinSet<Result<()>>,
    registry: &PluginRegistry,
    environment: &Option<EnvironmentConfig>,
    notification_settings: &Option<NotificationSettings>,
    immutable_store: Option<&Arc<dyn ImmutableStore>>,
    plugins: &HashMap<String, toml::Value>,
) -> Result<(Arc<dyn NotificationSender>, Option<NotificationService>)> {
    let mode = notification_settings
        .as_ref()
        .map_or("local", |ns| ns.mode.as_ref());
    match mode {
        "local" => {
            info!("Starting local notification service");
            let sender = Arc::new(crate::notification::local::NotificationSender::default());
            Ok((sender.clone(), Some(NotificationService::new(sender))))
        }
        plugin_name => {
            info!(plugin_name = plugin_name, "Creating notification plugin");

            // Get the plugin config from the plugins section
            let plugin_config = plugins
                .get(plugin_name)
                .cloned()
                .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));

            let context = NotificationPluginContext {
                environment: environment.clone(),
                immutable_store: immutable_store.cloned(),
            };

            let output = registry
                .create_notification(plugin_name, &plugin_config, &context)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("Failed to create notification plugin '{plugin_name}': {e}")
                })?;

            // Spawn background tasks for the receiver tasks from the plugin
            for task in output.receivers {
                lore_spawn!(endpoints, async move {
                    task.await.map_err(|e| {
                        anyhow::anyhow!("Notification plugin receiver background task failed: {e}")
                    })
                });
            }

            Ok((output.sender, None))
        }
    }
}

#[cfg(target_os = "linux")]
async fn log_base_address() {
    use tracing::warn;

    let executable =
        match std::env::current_exe().map(|executable| executable.to_str().map(|s| s.to_owned())) {
            Ok(executable) => executable,
            Err(e) => {
                warn!("Could not get path to current executable: {e:?}");
                return;
            }
        };

    if let Some(executable) = executable {
        match tokio::fs::read_to_string("/proc/self/maps").await {
            Ok(maps) => {
                let mut found: bool = false;
                // The format of the maps file is documented here: https://man7.org/linux/man-pages/man5/proc_pid_maps.5.html
                for line in maps.lines() {
                    // We're looking for the virtual address segments for our process that are
                    // readable, executable, and private.
                    if line.contains(executable.as_str()) && line.contains("r-xp") {
                        info!("Virtual address space mapping for {executable}: {}", line);
                        found = true;
                        break;
                    }
                }

                if !found {
                    warn!("Could not find mapping for {executable} in /proc/self/maps.");
                }
            }
            Err(e) => {
                warn!("Failed to read from /proc/self/maps: {e:?}");
            }
        }
    } else {
        warn!("Could not get executable path while logging base address.");
    }
}

#[cfg(feature = "seeding")]
async fn seed_local_store(settings: &LocalImmutableStoreSettings) -> Result<(), anyhow::Error> {
    const DEFAULT_BUFFER_SIZE: usize = 16_834;

    if std::env::var("LORE_SEEDING").is_ok() {
        let mut settings = settings.clone();
        // Delay flushing as long as possible during seeding to avoid unnecessary fsync's. This will
        // give us ~18 hours before we flush, so hopefully enough time for seeding to complete ;).
        settings.flush_delay_seconds = u16::MAX;
        let max_size = settings.max_size;
        let store = create_local_store(&settings, false /* flush background */).await?;

        if let Some(max_size) = max_size {
            info!("Seeding local store");
            let local_store = (store.clone() as Arc<dyn Any + Send + Sync>)
                .clone()
                .downcast::<lore_storage::local::immutable_store::LocalImmutableStore>()
                .expect("Could not downcast store");

            let margin = std::env::var("LORE_SEEDING_MARGIN")
                .map(|v| v.parse::<usize>().unwrap_or_default())
                .unwrap_or_default();

            let buffer = std::env::var("LORE_SEEDING_BUFFER")
                .map(|v| v.parse::<usize>().unwrap_or(DEFAULT_BUFFER_SIZE))
                .unwrap_or(DEFAULT_BUFFER_SIZE);

            let result = lore_revision::store::seeder::seed_local_store(
                local_store,
                max_size,
                margin,
                buffer,
            )
            .await
            .map_err(anyhow::Error::from);

            info!("Done seeding local store, flushing store to disk.");
            // Since we effectively disabled background flushing, we force a flush once seeding is
            // complete
            store.flush(true).await.map_err(anyhow::Error::from)?;
            info!("Done flushing store to disk.");

            result
        } else {
            warn!("Seeding was requested, but no max size was found.");
            Err(anyhow!("Missing max size on local store settings"))
        }
    } else {
        info!("Not seeding local store");
        Ok(())
    }
}

fn observe_task_lifecycles() {
    let meter = lore_telemetry::meter("lore.runtime");
    let spawned_tasks = meter
        .u64_counter("lore.runtime.tasks.spawned.total")
        .build();
    let inflight_tasks = meter
        .i64_up_down_counter("lore.runtime.tasks.running.total")
        .build();

    let callback = move |event: LoreTaskLifecycleEvent, spawn_location: &LoreTaskSpawnLocation| {
        let context_label = if let Some(context) = lore_revision::runtime::try_execution_context() {
            if let Some(lore_state) = context
                .caller_state()
                .cloned()
                .and_then(|any| ::std::sync::Arc::downcast::<ServerExecutionState>(any).ok())
            {
                lore_state.context_label
            } else {
                "<no server state>"
            }
        } else {
            "<no context>"
        };

        let labels = [
            KeyValue::new("context_label", context_label),
            KeyValue::new("spawn_file", spawn_location.file),
            KeyValue::new("spawn_line_number", spawn_location.line as i64),
        ];

        match event {
            LoreTaskLifecycleEvent::Started => {
                spawned_tasks.add(1, &labels);
                inflight_tasks.add(1, &labels);
            }
            LoreTaskLifecycleEvent::Completed | LoreTaskLifecycleEvent::Dropped => {
                inflight_tasks.add(-1, &labels);
            }
        }
    };

    if !set_task_lifecycle_callback(Box::new(callback)) {
        error!("Failed to set task events callback");
    }
}

/// [`ResourceDetectorProvider`] that augments an optional caller-supplied
/// provider with the resource detectors contributed by every compiled-in plugin.
///
/// The base server has no caller-supplied provider (`inner` is `None`); derived
/// binaries may set [`ServerConfig::resource_detector_provider`] to add detectors
/// on top of the plugin-contributed ones.
struct PluginResourceDetectorProvider<'a> {
    inner: Option<&'a dyn ResourceDetectorProvider>,
    registry: &'a PluginRegistry,
}

impl ResourceDetectorProvider for PluginResourceDetectorProvider<'_> {
    fn detectors(&self, runtime_handle: Handle) -> Vec<Box<dyn ResourceDetector>> {
        let mut detectors = self
            .inner
            .map(|provider| provider.detectors(runtime_handle.clone()))
            .unwrap_or_default();
        detectors.extend(self.registry.resource_detectors(runtime_handle));
        detectors
    }
}

async fn async_main(settings: (Settings, StringHash), config: ServerConfig) -> Result<()> {
    // Initialize metrics and tracing telemetry, returns a guard that will cleanup when it falls out
    // of scope
    let (settings, settings_hash) = settings;
    let runtime = runtime();
    let telemetry = settings.telemetry.clone().unwrap_or_default();
    let metrics_config = telemetry.metrics.clone().unwrap_or_default();
    let ua = &settings.server.user_agent;
    let user_agent_filter = Arc::new(
        UserAgentFilter::new(&ua.user_agent_patterns)
            .map_err(|e| anyhow::anyhow!("Invalid user_agent_patterns: {e}"))?
            .with_unknown_sample_rate(ua.unknown_user_agent_sample_rate),
    );

    // Initialize the plugin registry before telemetry: start with the
    // pre-populated registry from config, then register any build.rs-discovered
    // plugins. This must happen first so that every compiled-in plugin can
    // contribute OpenTelemetry resource detectors describing the deployment
    // environment it implies (e.g. AWS region, Nomad allocation).
    let mut plugin_registry = config.plugin_registry;
    plugins::register_all_plugins(&mut plugin_registry);

    let _guard = {
        let resource_detector_provider = PluginResourceDetectorProvider {
            inner: config.resource_detector_provider.as_deref(),
            registry: &plugin_registry,
        };
        TelemetryInitializer::from_config(
            &telemetry,
            runtime.clone(),
            Some(&resource_detector_provider),
        )?
        .init()?
    };

    observe_task_lifecycles();

    let up_gauge = lore_telemetry::meter("urc.server")
        .u64_gauge("urc.server.up")
        .build();
    let up_attributes = [
        KeyValue::new("version", LORE_LIBRARY_VERSION.to_string()),
        KeyValue::new("settings_hash", settings_hash.to_string()),
    ];
    up_gauge.record(0, &up_attributes);

    // We use /procfs to read the virtual address space of the process to ensure we're able to
    // calculate the correct offsets when symbolicating crashes. /procfs does not exist outside of
    // Linux.
    #[cfg(target_os = "linux")]
    log_base_address().await;

    // Enforce repository isolation in local store by default
    lore_storage::concurrency::LOCAL_ISOLATION.store(true, std::sync::atomic::Ordering::Release);

    let execution = execution_context();
    let runtime_monitor = tokio_metrics::RuntimeMonitor::new(&runtime);

    let metrics_bridge = OtelTokioRuntimeMetrics::new(&lore_telemetry::meter("tokio_runtime"));
    let frequency = Duration::from_millis(metrics_config.export_interval_millis);
    runtime.spawn(LORE_CONTEXT.scope(execution.clone(), async move {
        for metrics in runtime_monitor.intervals() {
            metrics_bridge.record(metrics);
            tokio::time::sleep(frequency).await;
        }
    }));

    if let Some(mode) = settings
        .environment
        .as_ref()
        .and_then(|env| env.config.as_ref())
        .and_then(|cfg| cfg.compression_mode.as_ref())
    {
        COMPRESSION_MODE.store(*mode as u32, std::sync::atomic::Ordering::Release);
    }

    info!(
        "Registered plugins - immutable stores: {:?}, mutable stores: {:?}, lock stores: {:?}, topology: {:?}, notification: {:?}",
        plugin_registry.list_immutable_store_plugins(),
        plugin_registry.list_mutable_store_plugins(),
        plugin_registry.list_lock_store_plugins(),
        plugin_registry.list_topology_plugins(),
        plugin_registry.list_notification_plugins(),
    );

    #[cfg(feature = "seeding")]
    {
        let mode = &settings.immutable_store.mode;

        // We're ok assuming that local store settings are present when seeding is enabled, if it's
        // not let it panic, and we can fix the configuration.
        let local_store_settings = match mode.as_str() {
            store_mode::LOCAL => settings
                .immutable_store
                .local
                .clone()
                .ok_or(anyhow!("Missing local immutable store settings")),
            store_mode::COMPOSITE => settings
                .immutable_store
                .composite
                .as_ref()
                .and_then(|s| s.local.local.clone())
                .ok_or(anyhow!("Missing composite store settings")),
            _ => {
                // If store mode is not local or composite, there's no local store settings to be
                // had.
                Err(anyhow!("No local store settings found"))
            }
        }?;

        seed_local_store(&local_store_settings).await?;
    }

    // Configure topology using plugin registry
    let topology_provider = settings
        .topology
        .as_ref()
        .and_then(|t| t.provider.plugin_name());

    let topology = configure_topology_with_registry(
        &plugin_registry,
        settings.topology.as_ref(),
        &settings.plugins,
    )?;

    if let Some(ref topo) = topology {
        info!(
            "Using topology plugin: {}",
            topology_provider.unwrap_or("none")
        );
        info!(
            "Topology supports refresh loop: {}",
            topo.supports_refresh_loop()
        );
    } else {
        info!("Running in single-node mode (no topology configured)");
    }

    let immutable_store =
        configure_immutable_store_via_plugin(&plugin_registry, &settings, topology.clone()).await?;

    let mutable_store =
        configure_mutable_store_via_plugin(&plugin_registry, &settings, immutable_store.clone())
            .await?;

    let lock_store = configure_lock_store_via_plugin(&plugin_registry, &settings)?;

    let connection_close_timeout =
        Duration::from_secs(settings.server.connection_close_timeout_seconds as u64);

    let is_maintenance = if std::env::var("LORE_SERVER_MAINTENANCE").is_ok_and(|v| v == "1") {
        info!(
            "Server is running in maintenance mode - only environment and health endpoints will be served"
        );
        true
    } else {
        false
    };

    let jwt_verifier = match settings.server.auth.as_ref() {
        Some(auth) => match auth.jwk.as_ref() {
            Some(jwk) => {
                let jwk_service = JwkServiceImpl::new(jwk.clone());
                jwk_service
                    .fetch_new_keys(None /* fetch all keys */)
                    .await?;
                let jwt_verifier = JwtVerifier {
                    jwk_service: Arc::new(jwk_service),
                    jwt_issuer: auth.jwt_issuer.clone(),
                    jwt_audience: auth.jwt_audience.clone(),
                };
                Some(jwt_verifier)
            }
            None => None,
        },
        None => None,
    };

    let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
    let mut endpoints = JoinSet::new();

    // Spawn signal listener outside the endpoint JoinSet so the drain
    // timeout only starts after the signal fires.
    lore_spawn!({
        let shutdown_tx = shutdown_tx.clone();
        listen_for_termination(shutdown_tx)
    });

    if !is_maintenance {
        let (notification, notification_service) = configure_notification(
            &mut endpoints,
            &plugin_registry,
            &settings.environment,
            &settings.notification,
            local_store().as_ref(),
            &settings.plugins,
        )
        .await?;

        // Build hook dispatcher: register build.rs-discovered hooks then config-provided hooks
        let hook_ctx = HookRegistrationContext {
            notification_sender: notification.clone(),
        };
        let mut hook_registry = HookRegistry::new();
        crate::hooks::register_all_hooks(&mut hook_registry, &hook_ctx);
        for callback in config.hook_registration_callbacks {
            callback(&mut hook_registry, &hook_ctx);
        }

        let enabled_hooks = hook_registry
            .create_enabled_hooks(&settings.hooks)
            .expect("Failed to create hooks from configuration");

        let hook_dispatcher = Arc::new(HookDispatcher::from_hooks_default(enabled_hooks));

        lore_spawn!(endpoints, {
            let immutable_store = immutable_store.clone();
            let mutable_store = mutable_store.clone();
            let lock_store = lock_store.clone();
            let jwt_verifier = jwt_verifier.clone();
            let settings = settings.clone();
            let notification = notification.clone();
            let user_agent_filter = user_agent_filter.clone();
            let shutdown_rx = _shutdown_rx.clone();

            let local_immutable_store = local_store().unwrap_or_else(|| {
                warn!("No local store available for gRPC server, operations requiring local store will route to the main store");
                immutable_store.clone()
            });

            launch_grpc_server(
                immutable_store,
                local_immutable_store,
                mutable_store,
                lock_store,
                jwt_verifier,
                settings,
                notification,
                notification_service,
                hook_dispatcher.clone(),
                user_agent_filter,
                shutdown_rx,
            )
        });

        if let Some(grpc_internal) = &settings.server.grpc_internal
            && grpc_internal.enabled
        {
            let security = validate_endpoint_security(
                "[server.grpc_internal]",
                grpc_internal.certificate.as_ref(),
                grpc_internal.verify_client_certs,
            )?;
            if security == EndpointSecurity::Untrusted {
                warn!(
                    "[server.grpc_internal] starting WITHOUT mTLS because verify_client_certs=false. \
                     The gRPC internal endpoint grants blanket access to every storage partition; \
                     only safe on isolated networks with no untrusted clients"
                );
            }
            lore_spawn!(endpoints, {
                let settings = settings.clone();
                let user_agent_filter = user_agent_filter.clone();
                let immutable_store = immutable_store.clone();
                let mutable_store = mutable_store.clone();
                let notification_sender = notification.clone();
                let hook_dispatcher = hook_dispatcher.clone();
                let shutdown_rx = _shutdown_rx.clone();
                launch_grpc_internal_server(
                    settings,
                    user_agent_filter,
                    immutable_store,
                    mutable_store,
                    notification_sender,
                    hook_dispatcher,
                    shutdown_rx,
                )
            });
        }
    } else {
        lore_spawn!(endpoints, {
            let settings = settings.clone();
            let shutdown_rx = _shutdown_rx.clone();
            launch_maintenance_grpc_server(settings, shutdown_rx)
        });
    }

    // the public facing QUIC server. Authentication is via JWT, so the
    // verify_client_certs flag is intentionally false in every shipped
    // config and is not validated here.
    if !is_maintenance
        && let Some(quic_settings) = settings.server.quic.as_ref()
        && quic_settings.enabled
    {
        lore_spawn!(endpoints, {
            let immutable_store = immutable_store.clone();
            let mutable_store = mutable_store.clone();
            let settings = settings.clone();
            let jwt_verifier = jwt_verifier.clone();
            let shutdown_rx = _shutdown_rx.clone();

            let quic_settings = settings
                .server
                .quic
                .ok_or(anyhow!("Missing Public QUIC config"))?;
            let request_handler_timeout = quic_settings
                .handler_timeout_seconds
                .map(Duration::from_secs);

            /// With 8 streams per connection this amounts to 4000 commands
            /// being processed in parallel per connection
            const DEFAULT_PROCESS_LIMIT: usize = 500;

            let local_immutable_store = local_store().unwrap_or_else(|| {
                warn!("No local store available for public QUIC server, operations requiring local store will route to the main store");
                immutable_store.clone()
            });

            info!(
                "Lore Public QUIC Server: Auth: {}",
                jwt_verifier.as_ref().map_or("no", |_| "yes")
            );

            launch_quinn_server(
                "public",
                Box::new(QuicPublicStreamHandler::new(
                    immutable_store,
                    local_immutable_store,
                    mutable_store,
                    jwt_verifier,
                    quic_settings
                        .connection_message_limit
                        .unwrap_or(DEFAULT_PROCESS_LIMIT),
                    request_handler_timeout,
                )),
                frequency,
                quic_settings,
                // User-facing endpoint: generate an ephemeral certificate when
                // none is configured so the server runs with zero config.
                true,
                shutdown_rx,
            )
        });
    }

    // the internal-only QUIC server. Hosts the ReplicationStoreService —
    // blanket storage access with no JWT layer — so the startup-time
    // validator demands mTLS unless verify_client_certs is explicitly
    // disabled.
    if let Some(quic_internal_settings) = settings.server.quic_internal.as_ref()
        && quic_internal_settings.enabled
    {
        let security = validate_endpoint_security(
            "[server.quic_internal]",
            quic_internal_settings.certificate.as_ref(),
            quic_internal_settings.verify_client_certs,
        )?;
        if security == EndpointSecurity::Untrusted {
            warn!(
                "[server.quic_internal] starting WITHOUT mTLS because verify_client_certs=false. \
                 The internal QUIC endpoint serves the replication store with blanket access \
                 to every storage partition; only safe on isolated networks with no untrusted \
                 clients"
            );
        }
        lore_spawn!(endpoints, {
            let immutable_store = immutable_store.clone();
            let settings = settings.clone();
            let shutdown_rx = _shutdown_rx.clone();

            let quic_settings = settings
                .server
                .quic_internal
                .ok_or(anyhow!("Missing Internal QUIC config"))?;
            let request_handler_timeout = quic_settings
                .handler_timeout_seconds
                .map(Duration::from_secs);

            let local_immutable_store = local_store().unwrap_or_else(|| {
                warn!("No local store available for internal QUIC server, ImmutableLocal* opcodes will route to the main store");
                immutable_store.clone()
            });

            launch_quinn_server(
                "internal",
                Box::new(QuicInternalStreamHandler::new(
                    immutable_store,
                    local_immutable_store,
                    quic_settings
                        .connection_message_limit
                        .unwrap_or(replication_store_service::DEFAULT_CLIENT_MESSAGE_LIMIT),
                    request_handler_timeout,
                )),
                frequency,
                quic_settings,
                // Internal QUIC endpoint requires a real (mTLS) certificate;
                // never fall back to an ephemeral one.
                false,
                shutdown_rx,
            )
        });
    }

    if !is_maintenance {
        if let Some(http_settings) = settings.server.http.as_ref()
            && http_settings.enabled
        {
            let lore_http_settings = LoreHttpServerSettings {
                port: http_settings.port,
                host: http_settings.host.clone(),
                max_file_size: http_settings.max_file_size,
                request_timeout_seconds: http_settings.request_timeout_seconds,
                request_body_timeout_seconds: http_settings.request_body_timeout_seconds,
                available_interval_seconds: http_settings.available_interval_seconds,
                available_timeout_seconds: http_settings.available_timeout_seconds,
                store_health_check: http_settings.store_health_check,
                presign: PresignSettings {
                    hmac_key: http_settings.presigned_url_hmac_key.clone(),
                    min_ttl_seconds: http_settings.presigned_url_min_ttl_seconds,
                    default_ttl_seconds: http_settings.presigned_url_default_ttl_seconds,
                    max_ttl_seconds: http_settings.presigned_url_max_ttl_seconds,
                },
                user_agent_filter: user_agent_filter.clone(),
            };
            let immutable_store = immutable_store.clone();
            let mutable_store = mutable_store.clone();
            let shutdown_rx = _shutdown_rx.clone();

            lore_spawn!(
                endpoints,
                launch_http_server(
                    lore_http_settings,
                    immutable_store,
                    mutable_store,
                    jwt_verifier,
                    shutdown_rx,
                )
            );
        }
    } else if let Some(http_settings) = settings.server.http.as_ref()
        && http_settings.enabled
    {
        lore_spawn!(endpoints, {
            let host = http_settings.host.clone();
            let port = http_settings.port;
            let user_agent_filter = user_agent_filter.clone();
            let shutdown_rx = _shutdown_rx.clone();
            LoreHttpServer::serve_maintenance(host, port, user_agent_filter, async move {
                let _ = shutdown_rx.clone().wait_for(|&v| v).await;
            })
        });
    }

    if let Some(topology) = &topology
        && topology.supports_refresh_loop()
    {
        let topology = topology.clone();
        lore_spawn!(endpoints, async move {
            topology.refresh_loop().await.map_err(anyhow::Error::from)
        });
    }

    lore_spawn!(async move {
        if let Some(store) = local_store() {
            crate::store::memory_stats_reporter(
                Arc::downgrade(&store),
                telemetry
                    .metrics
                    .map(|m| Duration::from_millis(m.sample_interval_millis)),
            )
            .await;
        }
    });

    up_gauge.record(1, &up_attributes);
    wait_for_shutdown(endpoints, shutdown_tx, connection_close_timeout).await?;
    up_gauge.record(0, &up_attributes);

    info!("Flushing stores");
    let _ = immutable_store.flush(true).await;
    let _ = mutable_store.flush(true).await;

    Ok(())
}

fn server_log_dispatch(level: lore_base::log::LoreLogLevel, location: &str, message: &str) {
    let correlation_id = lore_revision::lore::try_execution_context()
        .map(|e| e.dispatcher.correlation_id.clone())
        .unwrap_or_default();

    match level {
        lore_base::log::LoreLogLevel::None | lore_base::log::LoreLogLevel::Trace => {
            trace!(correlation_id, target = location, "{}", message);
        }
        lore_base::log::LoreLogLevel::Debug => {
            debug!(correlation_id, target = location, "{}", message);
        }
        lore_base::log::LoreLogLevel::Info => {
            info!(correlation_id, target = location, "{}", message);
        }
        lore_base::log::LoreLogLevel::Warn => {
            warn!(correlation_id, target = location, "{}", message);
        }
        lore_base::log::LoreLogLevel::Error => {
            error!(correlation_id, target = location, "{}", message);
        }
    }
}

#[cfg(test)]
mod tests {
    mod validate_endpoint_security {
        use std::path::PathBuf;

        use super::super::EndpointSecurity;
        use super::super::validate_endpoint_security;
        use crate::tls::CertificateSettings;

        const LABEL: &str = "[server.test]";

        fn mtls_triple() -> CertificateSettings {
            CertificateSettings {
                cert_file: PathBuf::from("cert.pem"),
                pkey_file: PathBuf::from("key.pem"),
                cert_chain: Some(PathBuf::from("ca.pem")),
            }
        }

        fn no_chain() -> CertificateSettings {
            CertificateSettings {
                cert_file: PathBuf::from("cert.pem"),
                pkey_file: PathBuf::from("key.pem"),
                cert_chain: None,
            }
        }

        #[test]
        fn full_mtls_triple_with_verify_yields_mtls() {
            let cert = mtls_triple();
            let security = validate_endpoint_security(LABEL, Some(&cert), true)
                .expect("full mTLS triple should be accepted");
            assert_eq!(security, EndpointSecurity::Mtls);
        }

        #[test]
        fn verify_off_yields_untrusted_even_with_full_triple() {
            // verify_client_certs is the operator's expressed intent; an
            // explicit `false` opts out regardless of what certs are
            // sitting on disk. The caller is expected to emit a startup
            // warning.
            let cert = mtls_triple();
            let security = validate_endpoint_security(LABEL, Some(&cert), false)
                .expect("verify_client_certs=false is always accepted");
            assert_eq!(security, EndpointSecurity::Untrusted);
        }

        #[test]
        fn verify_off_with_no_certs_yields_untrusted() {
            let security = validate_endpoint_security(LABEL, None, false)
                .expect("verify_client_certs=false is always accepted");
            assert_eq!(security, EndpointSecurity::Untrusted);
        }

        #[test]
        fn verify_on_with_no_certs_is_rejected() {
            let err = validate_endpoint_security(LABEL, None, true)
                .expect_err("default policy must refuse to start without mTLS");
            let message = err.to_string();
            assert!(message.contains(LABEL), "label must appear: {message}");
            assert!(message.contains("requires mTLS"), "got: {message}");
            assert!(
                message.contains("verify_client_certs"),
                "error must name the opt-out flag; got: {message}"
            );
        }

        #[test]
        fn verify_on_with_partial_cert_is_rejected() {
            // server-only TLS (no client-CA chain) is not mTLS. Don't
            // silently downgrade — refuse the config and name the missing
            // field so the operator can fix it.
            let cert = no_chain();
            let err = validate_endpoint_security(LABEL, Some(&cert), true)
                .expect_err("partial mTLS (no CA chain) must be rejected");
            let message = err.to_string();
            assert!(message.contains(LABEL), "label must appear: {message}");
            assert!(message.contains("partially configured"), "got: {message}");
            assert!(message.contains("cert_chain"), "got: {message}");
        }
    }
}
