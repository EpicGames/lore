// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A stub OIDC provider for the discovery and grant tests.
//!
//! Lives under `tests/support/` so cargo does not build it as a test target of its own; each
//! test binary pulls it in with `#[path]`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::Uri;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::routing::get;
use base64::Engine;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// A provider on a loopback port. `respond` gets the issuer URL and the number of the
/// request, counted from zero, and answers every path.
pub struct Provider {
    pub issuer: String,
    pub requests: Arc<AtomicUsize>,
    /// Requests the provider is still answering. A request leaves when its response is sent
    /// or its client disconnects.
    pub in_flight: Arc<AtomicUsize>,
}

/// Counts a request in flight until dropped.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub async fn spawn_provider(
    respond: impl Fn(&str, usize) -> Response + Clone + Send + Sync + 'static,
) -> Provider {
    spawn_gated_provider(watch::channel(true).1, respond).await
}

/// A provider that holds every response until `gate` turns true.
pub async fn spawn_gated_provider(
    gate: watch::Receiver<bool>,
    respond: impl Fn(&str, usize) -> Response + Clone + Send + Sync + 'static,
) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let issuer = format!(
        "http://{}{}",
        listener.local_addr().expect("local address"),
        unique_issuer_path()
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));

    let handler = {
        let issuer = issuer.clone();
        let requests = requests.clone();
        let in_flight = in_flight.clone();
        move || {
            let issuer = issuer.clone();
            let request = requests.fetch_add(1, Ordering::SeqCst);
            let respond = respond.clone();
            let mut gate = gate.clone();
            let in_flight = InFlight::enter(&in_flight);
            async move {
                let _in_flight = in_flight;
                gate.wait_for(|open| *open)
                    .await
                    .expect("gate sender alive");
                respond(&issuer, request)
            }
        }
    };
    let app = Router::new().fallback(get(handler));
    #[allow(clippy::disallowed_methods)] // Test-local server task.
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve the provider");
    });

    Provider {
        issuer,
        requests,
        in_flight,
    }
}

/// An issuer path no other provider in the process has. Discovery caches documents per issuer
/// until they are evicted, and the OS can hand a provider the port an earlier test's provider
/// used, so an issuer of only host and port could be answered from that test's document.
fn unique_issuer_path() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("/issuer-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The scheme, host and port of `url`.
pub fn origin(url: &str) -> String {
    url::Url::parse(url)
        .expect("a valid URL")
        .origin()
        .ascii_serialization()
}

pub fn document(issuer: &str, endpoint_base: &str) -> serde_json::Value {
    serde_json::json!({
        "issuer": issuer,
        "token_endpoint": format!("{endpoint_base}/token"),
        "jwks_uri": format!("{endpoint_base}/jwks"),
        "device_authorization_endpoint": format!("{endpoint_base}/device"),
        "revocation_endpoint": format!("{endpoint_base}/revoke"),
        "end_session_endpoint": format!("{endpoint_base}/logout"),
    })
}

pub fn json(value: &serde_json::Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

pub fn conforming(issuer: &str, _request: usize) -> Response {
    json(&document(issuer, issuer))
}

pub async fn wait_until(condition: impl Fn() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub async fn wait_for_requests(provider: &Provider, count: usize) {
    wait_until(
        || provider.requests.load(Ordering::SeqCst) >= count,
        "the provider to see the requests",
    )
    .await;
}

/// A request a [`RecordingProvider`] received.
#[derive(Clone, Debug)]
pub struct Received {
    pub method: Method,
    /// The request path relative to the issuer's path.
    pub path: String,
    pub headers: HeaderMap,
    pub body: String,
}

impl Received {
    /// The body as `application/x-www-form-urlencoded` fields.
    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// A provider on a loopback port that records every request, answering each from
/// `respond` with the issuer URL, the request, and its number among the requests to the
/// same path, counted from zero.
pub struct RecordingProvider {
    pub issuer: String,
    pub received: Arc<Mutex<Vec<Received>>>,
}

impl RecordingProvider {
    /// The requests to `path`, in order.
    pub fn requests_to(&self, path: &str) -> Vec<Received> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
            .cloned()
            .collect()
    }
}

pub async fn spawn_recording_provider(
    respond: impl Fn(&str, &Received, usize) -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    spawn_recording_provider_named("127.0.0.1", respond).await
}

/// A [`RecordingProvider`] whose issuer names the loopback port it listens on as `host`,
/// so two providers can sit on different hosts.
pub async fn spawn_recording_provider_named(
    host: &str,
    respond: impl Fn(&str, &Received, usize) -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().expect("local address").port();
    let issuer_path = unique_issuer_path();
    let issuer = format!("http://{host}:{port}{issuer_path}");
    let received: Arc<Mutex<Vec<Received>>> = Arc::default();

    let handler = {
        let issuer = issuer.clone();
        let received = received.clone();
        move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
            let issuer = issuer.clone();
            let received = received.clone();
            let respond = respond.clone();
            let path = uri
                .path()
                .strip_prefix(issuer_path.as_str())
                .unwrap_or(uri.path())
                .to_owned();
            async move {
                let request = Received {
                    method,
                    path,
                    headers,
                    body,
                };
                let same_path = {
                    let mut received = received.lock().unwrap();
                    let same_path = received
                        .iter()
                        .filter(|earlier| earlier.path == request.path)
                        .count();
                    received.push(request.clone());
                    same_path
                };
                respond(&issuer, &request, same_path)
            }
        }
    };
    let app = Router::new().fallback(any(handler));
    #[allow(clippy::disallowed_methods)] // Test-local server task.
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve the provider");
    });

    RecordingProvider { issuer, received }
}

/// An unsigned JWT carrying `claims`, enough for the client, which does not verify
/// signatures.
pub fn jwt(claims: &serde_json::Value) -> String {
    let encode = |part: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(part.to_string())
    };
    format!(
        "{}.{}.signature",
        encode(&serde_json::json!({"alg": "HS256", "typ": "JWT"})),
        encode(claims)
    )
}

/// An access token for `subject`, issued by `issuer`.
pub fn access_token(issuer: &str, subject: &str, name: &str) -> String {
    jwt(&serde_json::json!({
        "iss": issuer,
        "sub": subject,
        "name": name,
        "aud": ["lore.test.invalid"],
        "exp": 2_000_000_000,
        "iat": 1_900_000_000,
    }))
}

/// A loopback port that refuses connections, reserved while the value lives. The socket is bound
/// and never listens: a released port could be handed to a listener another test binds to port 0,
/// and a connection meant to be refused would reach it.
pub struct ClosedPort {
    _socket: tokio::net::TcpSocket,
    pub port: u16,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
}

pub fn closed_port() -> ClosedPort {
    let socket = tokio::net::TcpSocket::new_v4().expect("create a socket");
    socket
        .bind(([127, 0, 0, 1], 0).into())
        .expect("bind a port");
    let port = socket.local_addr().expect("the bound address").port();
    ClosedPort {
        _socket: socket,
        port,
        url: format!("http://127.0.0.1:{port}"),
    }
}
