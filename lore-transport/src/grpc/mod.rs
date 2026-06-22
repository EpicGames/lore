// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod admin_client;
mod environment_client;
mod lock_client;
mod repository_client;
mod revision_client;
mod storage_client;

use std::collections::HashMap;
use std::error::Error;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use http::header::AUTHORIZATION;
use lore_base::lore_debug;
use lore_base::lore_info;
use lore_base::lore_spawn;
use lore_base::lore_trace;
use lore_base::types::*;
use lore_base::version::LORE_LIBRARY_VERSION;
use lore_error_set::prelude::*;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tonic::Status;
use tonic::body::Body;
use tonic::codegen::InterceptedService;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::transport::ClientTlsConfig;
use tower::Layer;
use tower::Service;
use tower::ServiceBuilder;
use url::Url;

use crate::auth::exchange::auth_exchange;
use crate::auth::exchange::auth_exchange_custom_resource;
use crate::connection::Connection;
use crate::connection::RECONNECT_MAX_ATTEMPTS;
use crate::connection::RECONNECT_MAX_DELAY;
use crate::connection::RECONNECT_START_DELAY;
use crate::error::ProtocolError;
use crate::traits::*;
use crate::types::*;

// gRPC request metadata key for repo/partition IDs. Note: these keys are required to have
// "-bin" at the end since they store binary data. It's a gRPC thing.
pub const PARTITION_ID_KEY: &str = "lore-partition-bin";
pub const REPOSITORY_ID_KEY: &str = "urc-repository-id-bin";

// TODO(mjansson): This needs to be configurable, URC source should not be Epic specific
pub const CORRELATION_ID_HEADER: &str = "x-epic-correlation-id";
pub const REVISION_LIST_STRATEGY_HEADER: &str = "x-lore-revision-list-strategy";

const RETRY_START_BACKOFF_MS: u64 = 50;
const RETRY_MAX_BACKOFF_MS: u64 = 10_000;
const RETRY_MAX_ATTEMPTS: usize = 60;

fn grpc_retry() -> crate::util::Retry {
    crate::util::retry(
        RETRY_START_BACKOFF_MS,
        RETRY_MAX_BACKOFF_MS,
        RETRY_MAX_ATTEMPTS,
    )
}

#[derive(Default)]
pub struct GRPCAuth {
    pub remote_domain: String,
    pub authentication_token: String,
    pub authorization_token: String,
    pub refresher: Option<JoinHandle<()>>,
}

impl GRPCAuth {
    async fn new(
        auth_url: &str,
        remote_domain: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Arc<parking_lot::RwLock<Self>> {
        let remote_domain = remote_domain.to_string();

        let (authentication_token, authorization_token, resolved_identity) =
            auth_exchange(auth_url, &remote_domain, identity, repository).await;

        let auth = Arc::new(parking_lot::RwLock::new(Self {
            remote_domain: remote_domain.clone(),
            authentication_token,
            authorization_token,
            refresher: None,
        }));

        let auth_ref = Arc::downgrade(&auth);
        let refresher = Some(lore_spawn!(grpc_auth_refresher(
            auth_ref,
            auth_url.to_string(),
            remote_domain,
            resolved_identity,
            repository,
        )));

        {
            let mut auth = auth.write();
            auth.refresher = refresher;
        }

        auth
    }

    /// Builds a `GRPCAuth` whose authorization token is scoped to an arbitrary
    /// caller-supplied resource identifier. Used for endpoints whose authz
    /// model is not repository-based.
    async fn new_for_custom_resource(
        auth_url: &str,
        remote_domain: &str,
        identity: &str,
        resource_id: &str,
    ) -> Arc<parking_lot::RwLock<Self>> {
        let remote_domain = remote_domain.to_string();

        let (authentication_token, authorization_token, resolved_identity) =
            auth_exchange_custom_resource(auth_url, &remote_domain, identity, resource_id).await;

        let auth = Arc::new(parking_lot::RwLock::new(Self {
            remote_domain: remote_domain.clone(),
            authentication_token,
            authorization_token,
            refresher: None,
        }));

        let auth_ref = Arc::downgrade(&auth);
        let refresher = Some(lore_spawn!(grpc_auth_refresher_custom_resource(
            auth_ref,
            auth_url.to_string(),
            remote_domain,
            resolved_identity,
            resource_id.to_string(),
        )));

        {
            let mut auth = auth.write();
            auth.refresher = refresher;
        }

        auth
    }
}

type GRPCAuthRef = Arc<parking_lot::RwLock<GRPCAuth>>;

async fn grpc_auth_refresher(
    auth: Weak<parking_lot::RwLock<GRPCAuth>>,
    auth_url: String,
    remote_domain: String,
    identity: String,
    repository: RepositoryId,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        // Check if connection is still used
        let Some(auth) = auth.upgrade() else {
            return;
        };

        let (authentication_token, authorization_token, _) =
            auth_exchange(&auth_url, &remote_domain, &identity, repository).await;

        let mut auth = auth.write();
        auth.authentication_token = authentication_token;
        auth.authorization_token = authorization_token;
    }
}

async fn grpc_auth_refresher_custom_resource(
    auth: Weak<parking_lot::RwLock<GRPCAuth>>,
    auth_url: String,
    remote_domain: String,
    identity: String,
    resource_id: String,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        let Some(auth) = auth.upgrade() else {
            return;
        };

        let (authentication_token, authorization_token, _) =
            auth_exchange_custom_resource(&auth_url, &remote_domain, &identity, &resource_id).await;

        let mut auth = auth.write();
        auth.authentication_token = authentication_token;
        auth.authorization_token = authorization_token;
    }
}

pub fn inject_correlation_id(request: &mut tonic::Request<()>) -> Result<(), tonic::Status> {
    // In lore-transport, correlation_id is no longer available from ExecutionContext.
    // The correlation ID injection is now a no-op at this layer.
    // The caller (lore-core) can inject it if needed via a custom interceptor.
    let _ = request;
    Ok(())
}

pub fn inject_authorization(
    request: &mut tonic::Request<()>,
    token: &str,
) -> Result<(), tonic::Status> {
    if token.is_empty() {
        return Ok(());
    }
    let mut value = MetadataValue::from_str(&format!("Bearer {token}"))
        .map_err(|err| tonic::Status::failed_precondition(err.to_string()))?;
    value.set_sensitive(true);
    request.metadata_mut().insert(AUTHORIZATION.as_str(), value);
    Ok(())
}

pub fn inject_repository(
    request: &mut tonic::Request<()>,
    repository: RepositoryId,
) -> Result<(), tonic::Status> {
    if repository.is_zero() {
        return Ok(());
    }
    let value = MetadataValue::from_bytes(repository.data());
    request
        .metadata_mut()
        .append_bin(PARTITION_ID_KEY, value.clone());
    request.metadata_mut().append_bin(REPOSITORY_ID_KEY, value);

    Ok(())
}

#[derive(Clone)]
pub struct CorrelationInterceptor;

impl Interceptor for CorrelationInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        inject_correlation_id(&mut request)?;
        Ok(request)
    }
}

#[derive(Clone)]
pub struct AuthnInterceptor {
    pub auth: Arc<parking_lot::RwLock<GRPCAuth>>,
}

impl Interceptor for AuthnInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        inject_correlation_id(&mut request)?;
        inject_authorization(&mut request, self.auth.read().authentication_token.as_str())?;
        Ok(request)
    }
}

#[derive(Clone)]
pub struct AuthzInterceptor {
    pub repository: RepositoryId,
    pub auth: Arc<parking_lot::RwLock<GRPCAuth>>,
}

impl Interceptor for AuthzInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        inject_correlation_id(&mut request)?;
        inject_authorization(&mut request, self.auth.read().authorization_token.as_str())?;
        inject_repository(&mut request, self.repository)?;
        Ok(request)
    }
}

#[derive(Clone, Debug)]
pub struct RequestLoggerService<S> {
    inner: S,
}

impl<S> Service<http::Request<Body>> for RequestLoggerService<S>
where
    S: Service<http::Request<Body>> + Send + Sync + Clone + 'static,
    S::Error: Error + Send + Sync + 'static,
    S::Future: Send,
    S::Response: std::fmt::Debug,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut core::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            lore_debug!("gRPC request: {req:?}");
            let start = Instant::now();
            let result = inner.call(req).await;
            let elapsed = start.elapsed().as_millis();
            match &result {
                Ok(response) => {
                    lore_debug!("gRPC response: {response:?} ({elapsed} ms)");
                }
                Err(err) => {
                    lore_debug!("gRPC failure: {err:?} ({elapsed} ms)");
                }
            }
            result
        })
    }
}

pub struct RequestLoggerLayer {}
impl<S> Layer<S> for RequestLoggerLayer
where
    S: Service<http::Request<Body>>,
{
    type Service = RequestLoggerService<S>;

    fn layer(&self, service: S) -> Self::Service {
        RequestLoggerService { inner: service }
    }
}

pub type Channel = RequestLoggerService<tonic::transport::Channel>;
pub type UnauthenticatedService = InterceptedService<Channel, CorrelationInterceptor>;
pub type AuthenticatedService = InterceptedService<Channel, AuthnInterceptor>;
pub type AuthorizedService = InterceptedService<Channel, AuthzInterceptor>;

const GRPC_PORT_DEFAULT: u16 = 41337;
const GRPCS_PORT_DEFAULT: u16 = 443;

type AuthUrl = String;
type UserIdentity = String;
type ResourceId = String;

pub struct GRPCConnection {
    connection: Weak<Connection>,
    remote_url: Url,
    channel: parking_lot::RwLock<Channel>,
    auth: DashMap<(AuthUrl, UserIdentity, ResourceId), GRPCAuthRef>,
    reconnect: AtomicU32,
    reconnector: Semaphore,
}

impl GRPCConnection {
    pub fn channel(&self) -> Channel {
        self.channel.read().clone()
    }

    pub async fn repository_authz(
        &self,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> GRPCAuthRef {
        let key = (
            auth_url.to_string(),
            identity.to_string(),
            repository.to_string(),
        );

        if let Some(auth) = self.auth.get(&key) {
            return auth.clone();
        }

        let auth = GRPCAuth::new(
            auth_url,
            self.remote_url.host_str().unwrap_or_default(),
            identity,
            repository,
        )
        .await;

        self.auth.insert(key, auth.clone());
        auth
    }

    /// Obtains auth for a non-repository resource. The caller-supplied
    /// `resource` string is passed verbatim to the auth backend, scoping the
    /// resulting authorization token to that resource. The same string keys
    /// the connection-local cache.
    pub async fn custom_resource_authz(
        &self,
        auth_url: &str,
        identity: &str,
        resource: &str,
    ) -> GRPCAuthRef {
        let key = (
            auth_url.to_string(),
            identity.to_string(),
            resource.to_string(),
        );

        if let Some(auth) = self.auth.get(&key) {
            return auth.clone();
        }

        let auth = GRPCAuth::new_for_custom_resource(
            auth_url,
            self.remote_url.host_str().unwrap_or_default(),
            identity,
            resource,
        )
        .await;

        self.auth.insert(key, auth.clone());
        auth
    }

    pub async fn reconnect(&self, reconnect_id: u32) -> Result<Channel, ProtocolError> {
        let _permit = self.reconnector.acquire().await;

        let current_reconnect_id = self.reconnect.load(Ordering::Relaxed);
        if current_reconnect_id == 0 {
            // Reconnection failed, give up
            return Err(ProtocolError::from(lore_base::error::Disconnected));
        }
        if current_reconnect_id > reconnect_id {
            // Something else completed the reconnection already
            return Ok(self.channel());
        }

        let mut retry_count = 1;
        let mut retry = crate::util::retry(
            RECONNECT_START_DELAY,
            RECONNECT_MAX_DELAY,
            RECONNECT_MAX_ATTEMPTS,
        );

        loop {
            lore_info!(
                "Reconnecting to {} attempt {} / {}",
                self.remote_url,
                retry_count,
                RECONNECT_MAX_ATTEMPTS
            );

            let start = Instant::now();

            match connect_to_endpoint(self.remote_url.as_str()).await {
                Ok(channel) => {
                    let new_reconnect_id = 1 + self.reconnect.fetch_add(1, Ordering::Relaxed);

                    lore_debug!(
                        "gRPC reconnection to {} complete in {}ms ({reconnect_id} -> {new_reconnect_id})",
                        self.remote_url,
                        start.elapsed().as_millis()
                    );

                    {
                        let mut lock = self.channel.write();
                        *lock = channel.clone();
                    }

                    lore_info!("Reconnected to {}", self.remote_url);

                    return Ok(channel);
                }
                Err(err) => {
                    lore_debug!("Reconnect attempt failed: {err}");
                    if !retry.wait().await {
                        lore_debug!("Reconnect attempts exhausted, giving up");
                        // Indicate that any pending commands entering this flow should give up
                        self.reconnect.store(0, Ordering::Relaxed);
                        if let Some(connection) = self.connection.upgrade() {
                            connection.stale.store(true, Ordering::Relaxed);
                        }
                        return Err(err);
                    }
                }
            }

            retry_count += 1;
        }
    }
}

#[allow(clippy::type_complexity)]
static CONNECTION_MAP: OnceLock<Mutex<Option<HashMap<String, Arc<RwLock<Weak<GRPCConnection>>>>>>> =
    OnceLock::new();

async fn lock_connection(remote_url: &Url) -> Arc<RwLock<Weak<GRPCConnection>>> {
    let remote_url = remote_url.to_string();
    let mut map = CONNECTION_MAP.get_or_init(|| Mutex::new(None)).lock().await;
    if map.is_none() {
        map.replace(HashMap::new());
    }
    let map = map.as_mut().unwrap();
    if let Some(connection) = map.get(&remote_url) {
        return connection.clone();
    }

    let connection = Arc::new(RwLock::new(Weak::new()));
    map.insert(remote_url, connection.clone());

    connection
}

const HTTP2_KEEP_ALIVE_INTERVAL: u64 = 30;
const HTTP2_KEEP_ALIVE_TIMEOUT: u64 = 20;

static USER_AGENT: OnceLock<String> = OnceLock::new();

/// User agent string for gRPC connections. Reads from `LORE_USER_AGENT` env var,
/// falls back to a default value.
pub fn user_agent() -> &'static str {
    USER_AGENT
        .get_or_init(|| {
            std::env::var("LORE_USER_AGENT")
                .unwrap_or_else(|_| format!("lore-transport/{}", LORE_LIBRARY_VERSION.as_str()))
        })
        .as_str()
}

pub fn set_user_agent(name: String) -> bool {
    USER_AGENT.set(name).is_ok()
}

async fn connect_to_endpoint(remote: &str) -> Result<Channel, ProtocolError> {
    let mut endpoint = tonic::transport::Channel::from_shared(remote.to_string())
        .internal_with(|| format!("connect: {remote}"))?;

    let keep_alive_interval = std::env::var("LORE_HTTP2_KEEP_ALIVE_INTERVAL")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HTTP2_KEEP_ALIVE_INTERVAL);
    let keep_alive_timeout = std::env::var("LORE_HTTP2_KEEP_ALIVE_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HTTP2_KEEP_ALIVE_TIMEOUT);
    endpoint = endpoint
        .http2_keep_alive_interval(Duration::from_secs(keep_alive_interval))
        .keep_alive_timeout(Duration::from_secs(keep_alive_timeout));

    if remote.starts_with("https://") {
        endpoint = endpoint
            .tls_config(
                ClientTlsConfig::new()
                    .assume_http2(true)
                    .with_native_roots(),
            )
            .internal_with(|| format!("configuring TLS for {remote}"))?;
    }
    let user_agent = user_agent();
    endpoint = endpoint
        .user_agent(user_agent)
        .internal_with(|| format!("setting user agent for {remote}"))?;

    lore_trace!("Set user agent to {user_agent}");

    // Silent propagation of connection errors
    let channel = endpoint
        .connect()
        .await
        .internal_with(|| format!("gRPC connection to {remote}"))?;

    let channel = ServiceBuilder::new()
        .layer(RequestLoggerLayer {})
        .service(channel);

    Ok(channel)
}

pub async fn connect(
    connection: Weak<Connection>,
    remote_url: &str,
    reuse: bool,
) -> Result<Arc<GRPCConnection>, ProtocolError> {
    let parsed_url =
        Url::parse(remote_url).internal_with(|| format!("remote {remote_url} is invalid"))?;

    let host = parsed_url
        .host_str()
        .ok_or_else(|| ProtocolError::internal(format!("remote {remote_url} is invalid")))?;

    // Possible HTTPS schemes: urcs, grpcs, lores
    let (scheme, default_port) = if parsed_url.scheme().ends_with("s") {
        ("https", GRPCS_PORT_DEFAULT)
    } else {
        ("http", GRPC_PORT_DEFAULT)
    };

    // Use the LORE_GRPC_PORT env var as a temporary way to support
    // TLS-terminated LBs listening on a separate port in deployments
    let port = std::env::var("LORE_GRPC_PORT")
        .unwrap_or(parsed_url.port().unwrap_or(default_port).to_string());

    let remote = Url::parse(&format!("{scheme}://{host}:{port}"))
        .internal(&format!("remote {remote_url} is invalid"))?;

    let map_lock = lock_connection(&remote).await;
    let connection_lock = if reuse {
        let lock = map_lock.write().await;
        if let Some(connection) = lock.upgrade()
            && connection.reconnect.load(Ordering::Relaxed) > 0
        {
            lore_trace!("gRPC reusing previous connection: {remote}");
            return Ok(connection);
        }
        lore_trace!("gRPC found no previous valid connection: {remote}");
        Some(lock)
    } else {
        lore_trace!("gRPC unique connection: {remote}");
        None
    };

    lore_debug!("gRPC connecting: {remote}");

    let start = Instant::now();
    let channel = connect_to_endpoint(remote.as_str()).await?;

    lore_debug!(
        "gRPC connected in {}ms: {remote}",
        start.elapsed().as_millis()
    );

    let connection = Arc::new(GRPCConnection {
        connection,
        remote_url: remote,
        channel: parking_lot::RwLock::new(channel),
        auth: DashMap::new(),
        reconnect: AtomicU32::new(1),
        reconnector: Semaphore::new(1),
    });

    if let Some(mut lock) = connection_lock {
        *lock = Arc::downgrade(&connection);
        lore_trace!("Stored established gRPC connection");
    }

    Ok(connection)
}

async fn handle_error(retry: &mut crate::util::Retry, status: Status) -> Result<(), ProtocolError> {
    match status.code() {
        tonic::Code::ResourceExhausted => {
            if !retry.wait().await {
                return Err(ProtocolError::from(status));
            }
        }
        _ => return Err(ProtocolError::from(status)),
    }
    Ok(())
}

#[allow(clippy::unused_async)]
pub async fn storage_client(
    connection: Arc<GRPCConnection>,
    auth_url: &str,
    identity: &str,
    _repository: RepositoryId,
) -> Result<Arc<dyn Storage>, ProtocolError> {
    lore_trace!("Connecting gRPC storage client");

    let storage_client = storage_client::StorageService::new(connection.channel());

    let storage = GRPCStorage {
        connection,
        client: storage_client,
        auth_url: auth_url.to_string(),
        identity: identity.to_string(),
        session_counter: std::sync::atomic::AtomicU32::new(1),
        sessions: DashMap::new(),
    };

    lore_trace!("Connecting gRPC storage client complete");

    Ok(Arc::new(storage))
}

pub async fn revision_client(
    connection: Arc<GRPCConnection>,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Revision>, ProtocolError> {
    lore_trace!("Creating gRPC revision client");

    let revision_client = revision_client::RevisionService::new(
        connection.channel(),
        repository,
        connection
            .repository_authz(auth_url, identity, repository)
            .await,
    );

    let revision = GRPCRevision {
        connection,
        client: RwLock::new(revision_client),
        auth_url: auth_url.to_string(),
        identity: identity.to_string(),
        repository,
    };

    lore_trace!("Connecting gRPC revision client complete");

    Ok(Arc::new(revision))
}

pub async fn admin_client(
    connection: Arc<GRPCConnection>,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Admin>, ProtocolError> {
    lore_trace!("Creating gRPC admin client");

    let admin_client = admin_client::AdminService::new(
        connection.channel(),
        repository,
        connection
            .repository_authz(auth_url, identity, repository)
            .await,
    );

    let admin = GRPCAdmin {
        connection,
        client: RwLock::new(admin_client),
        auth_url: auth_url.to_string(),
        identity: identity.to_string(),
        repository,
    };

    lore_trace!("Connecting gRPC admin client complete");

    Ok(Arc::new(admin))
}

pub async fn repository_client(
    connection: Arc<GRPCConnection>,
    auth_url: &str,
    identity: &str,
) -> Result<Arc<dyn Repository>, ProtocolError> {
    lore_trace!("Connecting gRPC repository client");

    let repository_client = repository_client::RepositoryService::new(
        connection.channel(),
        connection
            .repository_authz(auth_url, identity, RepositoryId::default())
            .await,
    );

    let repository = GRPCRepository {
        connection,
        client: RwLock::new(repository_client),
        auth_url: auth_url.to_string(),
        identity: identity.to_string(),
    };

    lore_trace!("Connecting gRPC repository client complete");

    Ok(Arc::new(repository))
}

pub async fn lock_client(
    connection: Arc<GRPCConnection>,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Lock>, ProtocolError> {
    lore_trace!("Connecting gRPC lock client");

    let lock_client = lock_client::LockService::new(
        connection.channel(),
        repository,
        connection
            .repository_authz(auth_url, identity, repository)
            .await,
    );

    let lock = GRPCLock {
        connection,
        repository,
        client: RwLock::new(lock_client),
        auth_url: auth_url.to_string(),
        identity: identity.to_string(),
    };

    lore_trace!("Connecting gRPC lock client complete");

    Ok(Arc::new(lock))
}

pub fn environment_client(
    connection: Arc<GRPCConnection>,
) -> Result<Arc<dyn Environment>, ProtocolError> {
    lore_trace!("Connecting gRPC environment client");

    let environment_client = environment_client::EnvironmentService::new(connection.channel());

    let environment = GRPCEnvironment {
        connection,
        client: RwLock::new(environment_client),
    };

    lore_trace!("Connecting gRPC environment client complete");

    Ok(Arc::new(environment))
}

pub async fn storage(
    connection: Weak<Connection>,
    remote_url: &str,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
    index: usize,
) -> Result<Arc<dyn Storage>, ProtocolError> {
    // We open multiple storage connections, only reuse previous connections for the first
    let reuse = index == 0;
    let connection = connect(connection, remote_url, reuse).await?;
    storage_client(connection, auth_url, identity, repository).await
}

pub async fn revision(
    connection: Weak<Connection>,
    remote_url: &str,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Revision>, ProtocolError> {
    let connection = connect(connection, remote_url, true).await?;
    revision_client(connection, auth_url, identity, repository).await
}

pub async fn repository(
    connection: Weak<Connection>,
    remote_url: &str,
    auth_url: &str,
    identity: &str,
) -> Result<Arc<dyn Repository>, ProtocolError> {
    let connection = connect(connection, remote_url, true).await?;
    repository_client(connection, auth_url, identity).await
}

pub async fn lock(
    connection: Weak<Connection>,
    remote_url: &str,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Lock>, ProtocolError> {
    let connection = connect(connection, remote_url, true).await?;
    lock_client(connection, auth_url, identity, repository).await
}

pub async fn admin(
    connection: Weak<Connection>,
    remote_url: &str,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Admin>, ProtocolError> {
    let connection = connect(connection, remote_url, true).await?;
    admin_client(connection, auth_url, identity, repository).await
}

pub async fn environment(
    connection: Weak<Connection>,
    remote_url: &str,
) -> Result<Arc<dyn Environment>, ProtocolError> {
    let connection = connect(connection, remote_url, true).await?;
    environment_client(connection)
}

struct RequestScopedCounter {
    counter: Arc<AtomicU64>,
}

impl RequestScopedCounter {
    pub fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Release);
        Self { counter }
    }
}

impl Drop for RequestScopedCounter {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

struct GRPCAdmin {
    connection: Arc<GRPCConnection>,
    client: RwLock<admin_client::AdminService>,
    auth_url: String,
    identity: String,
    repository: RepositoryId,
}

impl GRPCAdmin {
    async fn reconnect(&self, reconnect_id: u32) -> Result<(), ProtocolError> {
        let channel = self.connection.reconnect(reconnect_id).await?;

        let mut lock = self.client.write().await;
        *lock = admin_client::AdminService::new(
            channel,
            self.repository,
            self.connection
                .repository_authz(
                    self.auth_url.as_str(),
                    self.identity.as_str(),
                    self.repository,
                )
                .await,
        );

        Ok(())
    }
}

#[async_trait]
impl Admin for GRPCAdmin {
    async fn obliterate(&self, address: Address) -> Result<(), ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.obliterate(address).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }
}

/// Storage protocol implementation over gRPC
struct GRPCStorage {
    connection: Arc<GRPCConnection>,
    client: storage_client::StorageService,
    /// Auth URL for token exchange.
    auth_url: String,
    /// Identity for token exchange.
    identity: String,
    /// Client-local session counter for monotonic session IDs.
    session_counter: std::sync::atomic::AtomicU32,
    /// Client-local session map: `session_id` -> context for metadata injection.
    /// Built at `session_start` time with the auth token, reused without lock reads.
    sessions: DashMap<u32, storage_client::GrpcSessionContext>,
}

impl GRPCStorage {
    /// Look up the cached session context. Cheap `DashMap` read, no auth token fetch.
    fn session_context(
        &self,
        session_id: u32,
    ) -> Result<storage_client::GrpcSessionContext, ProtocolError> {
        self.sessions
            .get(&session_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| ProtocolError::internal("gRPC session not found"))
    }
}

#[async_trait]
impl Storage for GRPCStorage {
    async fn session_start(
        &self,
        repository: RepositoryId,
        correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        let auth = self
            .connection
            .repository_authz(&self.auth_url, &self.identity, repository)
            .await;
        let token = auth.read().authorization_token.clone();

        let session_id = self
            .session_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sessions.insert(
            session_id,
            storage_client::GrpcSessionContext {
                repository,
                correlation_id: correlation_id.to_string(),
                auth_token: token,
            },
        );
        Ok(session_id)
    }

    async fn session_stop(&self, session_id: u32) -> Result<(), ProtocolError> {
        self.sessions.remove(&session_id);
        self.client.remove_session_streams(session_id);
        Ok(())
    }

    async fn get(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client.get(session_id, &ctx, address).await
    }

    async fn get_metadata(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client.get_metadata(session_id, &ctx, address).await
    }

    async fn put(
        &self,
        session_id: u32,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client
            .put(session_id, &ctx, address, fragment, payload)
            .await
    }

    async fn query(&self, session_id: u32, address: &[Address]) -> Result<Bytes, ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client.query(&ctx, address).await
    }

    async fn verify(
        &self,
        session_id: u32,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client.verify(&ctx, address, heal).await
    }

    async fn copy(
        &self,
        session_id: u32,
        source_repository: RepositoryId,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client
            .copy(
                session_id,
                &ctx,
                source_repository,
                source_address,
                target_context,
            )
            .await
    }

    async fn mutable_load(
        &self,
        session_id: u32,
        key: &Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client.mutable_load(&ctx, key, key_type).await
    }

    async fn mutable_store(
        &self,
        session_id: u32,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client.mutable_store(&ctx, key, value, key_type).await
    }

    async fn mutable_compare_and_swap(
        &self,
        session_id: u32,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let ctx = self.session_context(session_id)?;
        self.client
            .mutable_compare_and_swap(&ctx, key, expected, value, key_type)
            .await
    }
}

/// Revision protocol implementation over gRPC
struct GRPCRevision {
    connection: Arc<GRPCConnection>,
    client: RwLock<revision_client::RevisionService>,
    auth_url: String,
    identity: String,
    repository: RepositoryId,
}

impl GRPCRevision {
    async fn reconnect(&self, reconnect_id: u32) -> Result<(), ProtocolError> {
        let channel = self.connection.reconnect(reconnect_id).await?;

        let mut lock = self.client.write().await;
        *lock = revision_client::RevisionService::new(
            channel,
            self.repository,
            self.connection
                .repository_authz(
                    self.auth_url.as_str(),
                    self.identity.as_str(),
                    self.repository,
                )
                .await,
        );

        Ok(())
    }
}

#[async_trait]
impl Revision for GRPCRevision {
    async fn branch_create(
        &self,
        branch: BranchId,
        name: &str,
        category: &str,
        creator: &str,
        stack: &[BranchPoint],
    ) -> Result<Hash, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .branch_create(branch, name, category, creator, stack)
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn branch_delete(&self, branch: BranchId) -> Result<(), ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.branch_delete(branch).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn branch_query(
        &self,
        branch: Option<BranchId>,
        name: Option<&str>,
    ) -> Result<BranchQueryResponse, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.branch_query(branch, name).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn branch_push(
        &self,
        branch: BranchId,
        latest: Hash,
        force: bool,
        fast_forward_merge: bool,
    ) -> Result<BranchPushResponse, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .branch_push(branch, latest, force, fast_forward_merge)
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn branch_list(&self) -> Result<BranchListResponse, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.branch_list().await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn revision_list(
        &self,
        signature: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .revision_list(signature.clone())
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn branch_metadata_get(&self, branch: BranchId) -> Result<Hash, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.branch_metadata_get(branch).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn branch_metadata_set(
        &self,
        branch: BranchId,
        expected: Hash,
        new: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .branch_metadata_set(branch, expected, new)
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }
}

/// Repository protocol implementation over gRPC
struct GRPCRepository {
    connection: Arc<GRPCConnection>,
    client: RwLock<repository_client::RepositoryService>,
    auth_url: String,
    identity: String,
}

impl GRPCRepository {
    async fn reconnect(&self, reconnect_id: u32) -> Result<(), ProtocolError> {
        let channel = self.connection.reconnect(reconnect_id).await?;

        let mut lock = self.client.write().await;
        *lock = repository_client::RepositoryService::new(
            channel,
            self.connection
                .repository_authz(
                    self.auth_url.as_str(),
                    self.identity.as_str(),
                    RepositoryId::default(),
                )
                .await,
        );

        Ok(())
    }
}

#[async_trait]
impl Repository for GRPCRepository {
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        id: RepositoryId,
        name: &str,
        description: &str,
        default_branch_id: Context,
        default_branch_name: &str,
        creator: &str,
        created: u64,
    ) -> Result<RepositoryData, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .create(
                    id,
                    name,
                    description,
                    default_branch_id,
                    default_branch_name,
                    creator,
                    created,
                )
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn delete(&self, id: RepositoryId) -> Result<(), ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.delete(id).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn query(
        &self,
        id: Option<RepositoryId>,
        name: Option<&str>,
    ) -> Result<RepositoryData, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.query(id, name).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn list(&self) -> Result<Vec<RepositoryData>, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.list().await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn metadata_get(&self, id: RepositoryId) -> Result<Hash, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.metadata_get(id).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn metadata_set(
        &self,
        id: RepositoryId,
        expected: Hash,
        new: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .metadata_set(id, expected, new)
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }
}

/// Lock protocol implementation over gRPC
struct GRPCLock {
    connection: Arc<GRPCConnection>,
    client: RwLock<lock_client::LockService>,
    auth_url: String,
    identity: String,
    repository: RepositoryId,
}

impl GRPCLock {
    async fn reconnect(&self, reconnect_id: u32) -> Result<(), ProtocolError> {
        let channel = self.connection.reconnect(reconnect_id).await?;

        let mut lock = self.client.write().await;
        *lock = lock_client::LockService::new(
            channel,
            self.repository,
            self.connection
                .repository_authz(
                    self.auth_url.as_str(),
                    self.identity.as_str(),
                    self.repository,
                )
                .await,
        );

        Ok(())
    }
}

#[async_trait]
impl Lock for GRPCLock {
    async fn lock(
        &self,
        resources: &[LockResource],
        owner: Option<&str>,
    ) -> Result<Vec<LockData>, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.lock(resources, owner).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn query(
        &self,
        branch: Option<BranchId>,
        owner: Option<&str>,
        description: Option<&str>,
    ) -> Result<Vec<LockData>, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self
                .client
                .read()
                .await
                .query(branch, owner, description)
                .await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn status(&self, resources: &[LockResource]) -> Result<Vec<LockData>, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.status(resources).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }

    async fn unlock(&self, resources: &[LockResource]) -> Result<Vec<LockResource>, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.unlock(resources).await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }
}

/// Environment protocol implementation over gRPC
struct GRPCEnvironment {
    connection: Arc<GRPCConnection>,
    client: RwLock<environment_client::EnvironmentService>,
}

impl GRPCEnvironment {
    async fn reconnect(&self, reconnect_id: u32) -> Result<(), ProtocolError> {
        let channel = self.connection.reconnect(reconnect_id).await?;

        let mut lock = self.client.write().await;
        *lock = environment_client::EnvironmentService::new(channel);

        Ok(())
    }
}

#[async_trait]
impl Environment for GRPCEnvironment {
    async fn get(&self) -> Result<EnvironmentConfig, ProtocolError> {
        let reconnect_id = self.connection.reconnect.load(Ordering::Relaxed);
        loop {
            let result = self.client.read().await.get().await;
            match result {
                Err(ProtocolError::Disconnected(_)) => {
                    self.reconnect(reconnect_id).await?;
                }
                result => return result,
            }
        }
    }
}
