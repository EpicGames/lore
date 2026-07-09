// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use lore_proto::AdminServiceServer;
use lore_proto::LockServiceServer;
use lore_proto::lore::environment::v1::environment_service_server as environment_v1_server;
use lore_proto::lore::repository::v1::repository_service_server as repository_v1_server;
use lore_proto::lore::revision::v1::revision_service_server as revision_v1_server;
use lore_proto::lore::storage::v1::storage_service_server as storage_service_v1_server;
use lore_proto::lore::thin_client::v1::thin_client_service_server as thin_client_v1_server;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::environment::EnvironmentConfig;
use lore_revision::lock::LockStore;
use lore_revision::notification::NotificationSender;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_telemetry::grpc_tower_layer::GrpcMetricsLayer;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use serde::Deserialize;
use tonic::transport::Identity;
use tonic::transport::ServerTlsConfig;
use tonic::transport::server::Server;
use tower::ServiceBuilder;
use tower::layer::util::Stack;
use tower_http::classify::GrpcCode;
use tower_http::classify::GrpcErrorsAsFailures;
use tower_http::classify::SharedClassifier;
use tower_http::trace::TraceLayer;
use tracing::info;

use super::lock_service::LoreLockService;
use crate::auth::jwt::JwtVerifier;
use crate::auth::jwt_interceptor::JWTAuthnInterceptor;
use crate::auth::jwt_interceptor::JWTInterceptor;
use crate::correlation::layer::CorrelationIdLayer;
use crate::correlation::layer::CorrelationIdLayerBuilder;
use crate::correlation::layer::TraceLayerConfig;
use crate::correlation::span::MakeCorrelationIdSpan;
use crate::grpc::admin_service::LoreAdminService;
use crate::grpc::environment::LoreEnvironmentV1Service;
use crate::grpc::environment_service::LoreEnvironmentService;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::forwarded_requests::ForwardedRequestsSettings;
use crate::grpc::notification_service::NotificationService;
use crate::grpc::repository::LoreRepositoryV1Service;
use crate::grpc::repository_service::LoreRepositoryService;
use crate::grpc::revision::LoreRevisionV1Service;
use crate::grpc::revision_service::LoreRevisionService;
use crate::grpc::storage_service::LoreStorageService;
use crate::grpc::thinclient::LoreThinClientV1Service;
use crate::grpc::tower::grpc_response_trace::GrpcResponseTraceLayer;
use crate::grpc::tower::tracing::LoreTracingLayer;
use crate::hooks::HookDispatcher;
use crate::legacy::rpc::environment_service_server::EnvironmentServiceServer;
use crate::legacy::rpc::repository_service_server::RepositoryServiceServer;
use crate::legacy::rpc::revision_service_server::RevisionServiceServer;
use crate::legacy::rpc::storage_service_server::StorageServiceServer;

// Why Tower, why?
// Just try to make this type alias match the 'router' type in GrpcServerBuilder.
// Copy and paste from the rust compiler for sanity
type GrpcRouter = tonic::transport::server::Router<
    Stack<
        GrpcResponseTraceLayer,
        Stack<
            ServiceBuilder<Stack<GrpcMetricsLayer, tower::layer::util::Identity>>,
            Stack<
                LoreTracingLayer,
                Stack<
                    Stack<
                        TraceLayer<SharedClassifier<GrpcErrorsAsFailures>, MakeCorrelationIdSpan>,
                        CorrelationIdLayer,
                    >,
                    tower::layer::util::Identity,
                >,
            >,
        >,
    >,
>;

#[derive(Clone, Debug, Deserialize)]
pub struct GrpcServiceSettings {
    // max size of response payloads
    pub max_encoding_message_size: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GrpcPublicServicesSettings {
    pub lock_service: Option<GrpcServiceSettings>,
    pub forwarded_requests: Option<ForwardedRequestsSettings>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FeatureSettings {
    /// Size of revision history step blocks for accelerated lookups.
    /// Defaults to 100 if not specified.
    pub history_step_size: Option<u64>,
    /// Enables the persistent skip pointer (`revision_step_key`) that
    /// short-circuits `RevisionList` identifier resolution. Defaults to
    /// `true`. When disabled, identifier lookups fall through to a full
    /// `parent_self` walk and the push hook stops writing skip pointers.
    pub revision_step_keys: Option<bool>,
    /// Enables the persistent per-segment cache of pre-built
    /// `RevisionItem`s. Defaults to `true`. When disabled, the v1
    /// handler skips the cache fast path and its backfill rebuild;
    /// pushes stop populating cache entries.
    pub revision_list_cache: Option<bool>,
    /// Maximum source-side change count the v1 `RevisionDiff` 3-way
    /// handler accepts before aborting with
    /// `Status::resource_exhausted`. Bounds peak memory on the
    /// streaming 3-way path. Defaults to
    /// `DEFAULT_REVISION_DIFF_SOURCE_CAP` (100k items, ≈ 50 MB
    /// worst-case for the source `Vec` at ~500 B per `NodeChange`).
    /// SDK callers (`lore-capi`, `lore` CLI) bypass this cap.
    pub revision_diff_source_cap: Option<usize>,
    /// Permit count for the semaphore gating parallel
    /// `is_last_change_merged` history walks inside
    /// `revision::diff3`. Defaults to
    /// `lore_revision::revision::DEFAULT_HISTORY_WALK_CONCURRENCY`
    /// (24, set empirically — see comments in `revision::diff3`).
    /// Higher values cost RSS per concurrent walk because each
    /// holds an `Arc<State>` over a deserialised revision blob;
    /// the wall-clock benefit saturates well below 64.
    pub revision_diff_history_walk_concurrency: Option<usize>,
}

/// Toggles for `RevisionList` acceleration features. Resolved once at
/// server start from [`FeatureSettings`], then propagated through the
/// revision services to the read and write handlers.
#[derive(Clone, Copy, Debug)]
pub struct RevisionListAcceleration {
    /// Use the `revision_step_key` skip pointer (read + write).
    pub step_keys: bool,
    /// Use the per-segment cached page (read + backfill + write).
    pub list_cache: bool,
}

impl RevisionListAcceleration {
    pub fn from_feature(feature: &FeatureSettings) -> Self {
        Self {
            step_keys: feature.revision_step_keys.unwrap_or(true),
            list_cache: feature.revision_list_cache.unwrap_or(true),
        }
    }
}

impl Default for RevisionListAcceleration {
    fn default() -> Self {
        Self {
            step_keys: true,
            list_cache: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct GrpcServerBuilder<State>(State);

pub struct WantsEnvironment(());
impl GrpcServerBuilder<WantsEnvironment> {
    pub fn new() -> Self {
        Self(WantsEnvironment(()))
    }
    pub fn with_environment(
        self,
        environment: EnvironmentConfig,
    ) -> GrpcServerBuilder<WantsFeature> {
        GrpcServerBuilder(WantsFeature { environment })
    }
}

pub struct WantsFeature {
    environment: EnvironmentConfig,
}

impl GrpcServerBuilder<WantsFeature> {
    pub fn with_feature(self, feature: FeatureSettings) -> GrpcServerBuilder<WantsImmutableStore> {
        GrpcServerBuilder(WantsImmutableStore {
            environment: self.0.environment,
            feature,
        })
    }
}

pub struct WantsImmutableStore {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
}

impl GrpcServerBuilder<WantsImmutableStore> {
    pub fn with_immutable_store(
        self,
        immutable_store: Arc<dyn ImmutableStore>,
        local_store: Arc<dyn ImmutableStore>,
    ) -> GrpcServerBuilder<WantsMutableStore> {
        GrpcServerBuilder(WantsMutableStore {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store,
            local_store,
        })
    }
}

pub struct WantsMutableStore {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
}

impl GrpcServerBuilder<WantsMutableStore> {
    pub fn with_mutable_store(
        self,
        mutable_store: Arc<dyn MutableStore>,
    ) -> GrpcServerBuilder<MaybeLockStore> {
        GrpcServerBuilder(MaybeLockStore {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store,
        })
    }
}

pub struct MaybeLockStore {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
}

impl GrpcServerBuilder<MaybeLockStore> {
    pub fn with_lock_store(
        self,
        lock_store: Option<Arc<dyn LockStore>>,
    ) -> GrpcServerBuilder<WantsNotification> {
        GrpcServerBuilder(WantsNotification {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store: self.0.mutable_store,
            lock_store,
        })
    }
}

pub struct WantsNotification {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
}

impl GrpcServerBuilder<WantsNotification> {
    pub fn with_notification(
        self,
        sender: Arc<dyn NotificationSender>,
        service: Option<NotificationService>,
    ) -> GrpcServerBuilder<MaybeHookDispatcher> {
        GrpcServerBuilder(MaybeHookDispatcher {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store: self.0.mutable_store,
            lock_store: self.0.lock_store,
            notification_sender: sender,
            notification_service: service,
        })
    }
}

pub struct MaybeHookDispatcher {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
    notification_sender: Arc<dyn NotificationSender>,
    notification_service: Option<NotificationService>,
}

impl GrpcServerBuilder<MaybeHookDispatcher> {
    pub fn with_hook_dispatcher(
        self,
        hook_dispatcher: Arc<HookDispatcher>,
    ) -> GrpcServerBuilder<WantsTlsConfig> {
        GrpcServerBuilder(WantsTlsConfig {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store: self.0.mutable_store,
            lock_store: self.0.lock_store,
            notification_sender: self.0.notification_sender,
            notification_service: self.0.notification_service,
            hook_dispatcher,
        })
    }
}

pub struct WantsTlsConfig {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
    notification_sender: Arc<dyn NotificationSender>,
    notification_service: Option<NotificationService>,
    hook_dispatcher: Arc<HookDispatcher>,
}

impl GrpcServerBuilder<WantsTlsConfig> {
    pub fn with_tls_config(
        self,
        cert_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        cert_chain_path: Option<PathBuf>,
    ) -> Result<GrpcServerBuilder<WantsAdminEndpoints>> {
        let tls_config = if let Some(key_path) = key_path {
            let cert_path =
                cert_chain_path.unwrap_or(cert_path.ok_or(anyhow!("Missing TLS cert path"))?);
            info!(
                "Loading TLS certs - cert: {:?} key: {:?}",
                cert_path, key_path
            );
            let cert = std::fs::read(cert_path)?;
            info!("Loading TLS key: {:?}", key_path);
            let key = std::fs::read(key_path)?;
            let identity = Identity::from_pem(cert, key);

            Some(
                ServerTlsConfig::new()
                    .identity(identity)
                    .client_auth_optional(true),
            )
        } else {
            None
        };

        Ok(GrpcServerBuilder(WantsAdminEndpoints {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store: self.0.mutable_store,
            lock_store: self.0.lock_store,
            hook_dispatcher: self.0.hook_dispatcher,
            notification_sender: self.0.notification_sender,
            notification_service: self.0.notification_service,
            tls_config,
        }))
    }
}

pub struct WantsAdminEndpoints {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
    hook_dispatcher: Arc<HookDispatcher>,
    notification_sender: Arc<dyn NotificationSender>,
    notification_service: Option<NotificationService>,
    tls_config: Option<ServerTlsConfig>,
}

impl GrpcServerBuilder<WantsAdminEndpoints> {
    pub fn with_admin_endpoints(
        self,
        settings: HashMap<String, String>,
        features: Vec<String>,
    ) -> GrpcServerBuilder<WantsHttp2Config> {
        let admin_svc = LoreAdminService::new(
            settings,
            features,
            self.0.immutable_store.clone(),
            self.0.mutable_store.clone(),
            self.0.notification_sender.clone(),
            self.0.hook_dispatcher.clone(),
        );
        GrpcServerBuilder(WantsHttp2Config {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store: self.0.mutable_store,
            lock_store: self.0.lock_store,
            notification_sender: self.0.notification_sender,
            notification_service: self.0.notification_service,
            hook_dispatcher: self.0.hook_dispatcher,
            tls_config: self.0.tls_config,
            admin_svc,
        })
    }
}

pub struct WantsHttp2Config {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
    notification_sender: Arc<dyn NotificationSender>,
    notification_service: Option<NotificationService>,
    hook_dispatcher: Arc<HookDispatcher>,
    tls_config: Option<ServerTlsConfig>,
    admin_svc: LoreAdminService,
}

impl GrpcServerBuilder<WantsHttp2Config> {
    pub fn with_http2_config(
        self,
        http2_keep_alive_interval: Option<Duration>,
        http2_keep_alive_timeout: Option<Duration>,
        request_handler_timeout: Duration,
        service_settings: Option<GrpcPublicServicesSettings>,
        user_agent_filter: Arc<UserAgentFilter>,
        forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
    ) -> GrpcServerBuilder<MaybeJwtVerifier> {
        GrpcServerBuilder(MaybeJwtVerifier {
            environment: self.0.environment,
            feature: self.0.feature,
            immutable_store: self.0.immutable_store,
            local_store: self.0.local_store,
            mutable_store: self.0.mutable_store,
            lock_store: self.0.lock_store,
            notification_sender: self.0.notification_sender,
            notification_service: self.0.notification_service,
            hook_dispatcher: self.0.hook_dispatcher,
            tls_config: self.0.tls_config,
            admin_svc: self.0.admin_svc,
            http2_keep_alive_interval,
            http2_keep_alive_timeout,
            request_handler_timeout,
            service_settings,
            user_agent_filter,
            forwarded_requests,
        })
    }
}

pub struct MaybeJwtVerifier {
    environment: EnvironmentConfig,
    feature: FeatureSettings,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    lock_store: Option<Arc<dyn LockStore>>,
    notification_sender: Arc<dyn NotificationSender>,
    notification_service: Option<NotificationService>,
    hook_dispatcher: Arc<HookDispatcher>,
    tls_config: Option<ServerTlsConfig>,
    admin_svc: LoreAdminService,
    http2_keep_alive_interval: Option<Duration>,
    http2_keep_alive_timeout: Option<Duration>,
    request_handler_timeout: Duration,
    service_settings: Option<GrpcPublicServicesSettings>,
    user_agent_filter: Arc<UserAgentFilter>,
    forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
}

impl GrpcServerBuilder<MaybeJwtVerifier> {
    fn make_lock_service(
        services_settings: &Option<GrpcPublicServicesSettings>,
        inner: LoreLockService,
    ) -> LockServiceServer<LoreLockService> {
        let mut lock_service = LockServiceServer::new(inner);

        if let Some(lock_service_settings) = services_settings
            .as_ref()
            .and_then(|s| s.lock_service.as_ref())
            && let Some(max_encoding_message_size) = lock_service_settings.max_encoding_message_size
        {
            lock_service = lock_service.max_encoding_message_size(max_encoding_message_size);
        }

        lock_service
    }

    pub fn with_jwt_verifier(
        self,
        jwt_verifier: Option<JwtVerifier>,
    ) -> Result<GrpcServerBuilder<WantsAddress>> {
        let storage_svc = LoreStorageService::new(
            self.0.immutable_store.clone(),
            self.0.local_store.clone(),
            self.0.mutable_store.clone(),
        );
        let history_step_size = self
            .0
            .feature
            .history_step_size
            .unwrap_or(DEFAULT_HISTORY_STEP_SIZE);
        let acceleration = RevisionListAcceleration::from_feature(&self.0.feature);
        let rpc_timeout = self.0.request_handler_timeout;
        let revision_svc = ServiceBuilder::new().service(LoreRevisionService::new(
            self.0.immutable_store.clone(),
            self.0.mutable_store.clone(),
            self.0.notification_sender.clone(),
            self.0.hook_dispatcher.clone(),
            history_step_size,
            acceleration,
            rpc_timeout,
        ));
        let revision_v1_svc = LoreRevisionV1Service::new(
            self.0.immutable_store.clone(),
            self.0.mutable_store.clone(),
            self.0.notification_sender.clone(),
            self.0.hook_dispatcher.clone(),
            history_step_size,
            acceleration,
            self.0.forwarded_requests.clone(),
            rpc_timeout,
        );
        let revision_diff_config = crate::grpc::thinclient::v1::revision_diff::RevisionDiffConfig {
            source_cap: self.0.feature.revision_diff_source_cap.unwrap_or(
                crate::grpc::thinclient::v1::revision_diff::DEFAULT_REVISION_DIFF_SOURCE_CAP,
            ),
            history_walk_concurrency: self.0.feature.revision_diff_history_walk_concurrency,
        };
        let thin_client_v1_svc = LoreThinClientV1Service::new(
            self.0.immutable_store.clone(),
            self.0.mutable_store.clone(),
            rpc_timeout,
            revision_diff_config,
        );
        let repository_svc = LoreRepositoryService::new(
            self.0.environment.clone(),
            self.0.immutable_store.clone(),
            self.0.mutable_store.clone(),
            self.0.hook_dispatcher.clone(),
            rpc_timeout,
        );
        let repository_v1_svc = LoreRepositoryV1Service::new(
            self.0.environment.clone(),
            self.0.immutable_store.clone(),
            self.0.mutable_store.clone(),
            self.0.hook_dispatcher.clone(),
            rpc_timeout,
        );

        let environment_svc = LoreEnvironmentService::new(self.0.environment.clone());
        let environment_v1_svc = LoreEnvironmentV1Service::new(self.0.environment);
        let lock_svc = match self.0.lock_store {
            Some(lock_store) => {
                info!("Enabling LockService");
                Some(LoreLockService::new(
                    lock_store.clone(),
                    self.0.notification_sender.clone(),
                    rpc_timeout,
                ))
            }
            None => None,
        };
        let metrics_layer =
            tower::ServiceBuilder::new().layer(GrpcMetricsLayer::new(self.0.user_agent_filter));
        let mut server = Server::builder()
            .http2_keepalive_interval(self.0.http2_keep_alive_interval)
            .http2_keepalive_timeout(self.0.http2_keep_alive_timeout);
        if let Some(tls_config) = self.0.tls_config {
            server = server.tls_config(tls_config)?;
        }

        let mut admin_svc = self.0.admin_svc;
        admin_svc.set_jwt_verifier(jwt_verifier.clone());
        admin_svc.set_rpc_timeout(rpc_timeout);
        let trace_layer_config = {
            let mut config = TraceLayerConfig::default();
            config.grpc_codes_as_success.push(GrpcCode::Unauthenticated);
            config
        };
        let mut router = server
            .layer(
                CorrelationIdLayerBuilder::new()
                    .with_grpc_tracer(trace_layer_config)
                    .build(),
            )
            .layer(LoreTracingLayer {})
            .layer(metrics_layer)
            .layer(GrpcResponseTraceLayer {});

        let mut router = router.add_service(AdminServiceServer::new(admin_svc));

        if let Some(jwt_verifier) = jwt_verifier.as_ref() {
            let jwt_interceptor = JWTInterceptor::new(jwt_verifier);
            // TODO(UCS-13506): Placeholder authn verifier until separate authz flow for repository service is in place
            let jwt_authn_interceptor = JWTAuthnInterceptor::new(jwt_verifier);
            router = router
                .add_service(StorageServiceServer::with_interceptor(
                    storage_svc.clone(),
                    jwt_interceptor.clone(),
                ))
                .add_service(
                    storage_service_v1_server::StorageServiceServer::with_interceptor(
                        storage_svc,
                        jwt_interceptor.clone(),
                    ),
                )
                .add_service(RevisionServiceServer::with_interceptor(
                    revision_svc,
                    jwt_interceptor.clone(),
                ))
                .add_service(revision_v1_server::RevisionServiceServer::with_interceptor(
                    revision_v1_svc,
                    jwt_interceptor.clone(),
                ))
                .add_service(
                    thin_client_v1_server::ThinClientServiceServer::with_interceptor(
                        thin_client_v1_svc,
                        jwt_interceptor.clone(),
                    ),
                )
                .add_service(RepositoryServiceServer::with_interceptor(
                    repository_svc,
                    // TODO(UCS-13506): Placeholder authn verifier until separate authz flow for repository service is in place
                    jwt_authn_interceptor.clone(),
                ))
                .add_service(
                    repository_v1_server::RepositoryServiceServer::with_interceptor(
                        repository_v1_svc,
                        jwt_authn_interceptor.clone(),
                    ),
                )
                .add_service(EnvironmentServiceServer::new(environment_svc))
                .add_service(environment_v1_server::EnvironmentServiceServer::new(
                    environment_v1_svc,
                ));

            // Locks require auth, so set that up here
            if let Some(lock_svc) = lock_svc {
                let lock_service = Self::make_lock_service(&self.0.service_settings, lock_svc);
                let intercepted_service = tonic::service::interceptor::InterceptedService::new(
                    lock_service,
                    jwt_interceptor.clone(),
                );
                router = router.add_service(intercepted_service);
            }

            // Notifications require auth
            if let Some(notification_service) = self.0.notification_service {
                router = router.add_service(
                    lore_notification::NotificationServiceServer::with_interceptor(
                        notification_service,
                        jwt_interceptor.clone(),
                    ),
                );
            }
        } else {
            router = router
                .add_service(StorageServiceServer::new(storage_svc.clone()))
                .add_service(storage_service_v1_server::StorageServiceServer::new(
                    storage_svc,
                ))
                .add_service(RevisionServiceServer::new(revision_svc))
                .add_service(revision_v1_server::RevisionServiceServer::new(
                    revision_v1_svc,
                ))
                .add_service(thin_client_v1_server::ThinClientServiceServer::new(
                    thin_client_v1_svc,
                ))
                .add_service(RepositoryServiceServer::new(repository_svc))
                .add_service(repository_v1_server::RepositoryServiceServer::new(
                    repository_v1_svc,
                ))
                .add_service(EnvironmentServiceServer::new(environment_svc))
                .add_service(environment_v1_server::EnvironmentServiceServer::new(
                    environment_v1_svc,
                ));
            if let Some(lock_svc) = lock_svc {
                let lock_service = Self::make_lock_service(&self.0.service_settings, lock_svc);
                router = router.add_service(lock_service);
            }
            if let Some(notification_service) = self.0.notification_service {
                router = router.add_service(lore_notification::NotificationServiceServer::new(
                    notification_service,
                ));
            }
        }
        Ok(GrpcServerBuilder(WantsAddress { router }))
    }
}

pub struct WantsAddress {
    router: GrpcRouter,
}

impl GrpcServerBuilder<WantsAddress> {
    pub async fn serve(self, addr: SocketAddr, signal: impl Future<Output = ()>) -> Result<()> {
        self.0.router.serve_with_shutdown(addr, signal).await?;
        Ok(())
    }
}

/// Serves a minimal gRPC server with only the environment endpoint in maintenance mode.
/// The environment endpoint returns UNAVAILABLE status to signal that the server is in
/// maintenance.
pub async fn serve_maintenance(
    environment: EnvironmentConfig,
    addr: SocketAddr,
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    cert_chain_path: Option<PathBuf>,
    signal: impl Future<Output = ()>,
) -> Result<()> {
    let environment_svc = LoreEnvironmentService::maintenance(environment.clone());
    let environment_v1_svc = LoreEnvironmentV1Service::maintenance(environment);

    let mut server = Server::builder();
    if let Some(key_path) = key_path {
        let cert_path =
            cert_chain_path.unwrap_or(cert_path.ok_or(anyhow!("Missing TLS cert path"))?);
        info!(
            "Loading maintenance TLS certs - cert: {:?} key: {:?}",
            cert_path, key_path
        );
        let cert = std::fs::read(cert_path)?;
        let key = std::fs::read(key_path)?;
        let identity = Identity::from_pem(cert, key);
        let tls_config = ServerTlsConfig::new()
            .identity(identity)
            .client_auth_optional(true);
        server = server.tls_config(tls_config)?;
    }

    server
        .add_service(EnvironmentServiceServer::new(environment_svc))
        .add_service(environment_v1_server::EnvironmentServiceServer::new(
            environment_v1_svc,
        ))
        .serve_with_shutdown(addr, signal)
        .await?;

    Ok(())
}
