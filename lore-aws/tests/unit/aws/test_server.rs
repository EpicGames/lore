// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A scripted HTTP/1.1 server, so the attempt loop can be tested end to end.
//!
//! Hand-rolled over a [`TcpListener`] rather than built on a server library: the client is
//! HTTP/1.1-only, the exchange is a head, a `Content-Length` body and a canned reply, and a stub that
//! adds no dependency keeps the point of this crate intact. It records every request it receives, which
//! is what lets a test assert *what was signed* rather than only what came back.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// A request the stub received.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// A header's value, matched case-insensitively as HTTP requires.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("<not text>")
    }
}

/// One scripted reply.
#[derive(Debug, Clone)]
pub struct StubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Sent as `Transfer-Encoding: chunked`, in pieces of this size, with no `Content-Length`.
    pub chunked: Option<usize>,
    /// Close the connection instead of answering, which reaches the client as a transport failure.
    pub hangup: bool,
    /// How long to wait before each piece of a chunked reply, so its body arrives over time.
    pub pause: Option<Duration>,
}

impl StubResponse {
    pub fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            chunked: None,
            hangup: false,
            pause: None,
        }
    }

    /// Take the request and drop the connection without answering.
    ///
    /// The one failure the client still retries on its own: a request that never got an answer says
    /// nothing about whether the service is busy, and redoing it costs one request rather than the
    /// whole store operation around it.
    pub fn hangup() -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            chunked: None,
            hangup: true,
            pause: None,
        }
    }

    /// A reply whose length the client cannot know in advance, which is what a `Content-Length`
    /// check alone does not bound.
    pub fn chunked(status: u16, body: Vec<u8>, chunk: usize) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
            chunked: Some(chunk),
            hangup: false,
            pause: None,
        }
    }

    pub fn ok(body: &str) -> Self {
        Self::new(200, body)
    }

    /// A `DynamoDB`-shaped error reply.
    pub fn dynamo_error(status: u16, code: &str) -> Self {
        Self::new(
            status,
            &format!(r#"{{"__type":"com.amazonaws.dynamodb.v20120810#{code}"}}"#),
        )
    }

    /// An S3-shaped error reply.
    pub fn s3_error(status: u16, code: &str) -> Self {
        Self::new(
            status,
            &format!("<Error><Code>{code}</Code><RequestId>STUB-REQ</RequestId></Error>"),
        )
    }

    /// A reply with no body at all, as S3 answers a `HEAD`.
    pub fn empty(status: u16) -> Self {
        Self::new(status, "")
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Wait `pause` before each piece of a chunked reply.
    pub fn paced(mut self, pause: Duration) -> Self {
        self.pause = Some(pause);
        self
    }
}

/// Builds a reply from the request it answers.
type Responder = Arc<dyn Fn(&RecordedRequest) -> StubResponse + Send + Sync>;

/// How the stub answers.
#[derive(Clone)]
enum Mode {
    /// Reply from the script, repeating its last entry once exhausted.
    Scripted,
    /// Accept, read, and never answer — for exhausting an operation's budget.
    Silent,
    /// Reply with what the responder builds from each request, whatever order they arrive in.
    Responding(Responder),
}

/// A running stub.
///
/// Dropping it aborts the accept loop, so a test leaves no listener behind.
pub struct StubServer {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    accept_loop: JoinHandle<()>,
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.accept_loop.abort();
    }
}

impl StubServer {
    /// Start a stub that answers from `script`.
    pub async fn start(script: Vec<StubResponse>) -> Self {
        Self::spawn(script, Mode::Scripted).await
    }

    /// Start a stub that reads requests and never answers them.
    pub async fn silent() -> Self {
        Self::spawn(Vec::new(), Mode::Silent).await
    }

    /// Start a stub that answers each request with what `respond` builds from it.
    pub async fn responding(
        respond: impl Fn(&RecordedRequest) -> StubResponse + Send + Sync + 'static,
    ) -> Self {
        Self::spawn(Vec::new(), Mode::Responding(Arc::new(respond))).await
    }

    async fn spawn(script: Vec<StubResponse>, mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port is available");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter().collect::<Vec<_>>()));
        let served = Arc::new(Mutex::new(0usize));

        let accept_loop = {
            let recorded = recorded.clone();

            // Deliberately not `lore_spawn_net!`: the stub is the test's peer, and putting it on the
            // net runtime would blur what the client's own dispatch is doing there.
            #[allow(clippy::disallowed_methods)]
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let recorded = recorded.clone();
                    let script = script.clone();
                    let served = served.clone();
                    let mode = mode.clone();

                    #[allow(clippy::disallowed_methods)]
                    tokio::spawn(async move {
                        // A connection failing mid-exchange is the client hanging up, which several
                        // tests do on purpose.
                        let _ = serve(stream, recorded, script, served, mode).await;
                    });
                }
            })
        };

        Self {
            addr,
            recorded,
            accept_loop,
        }
    }

    /// The endpoint to point a client at.
    pub fn endpoint(&self) -> url::Url {
        url::Url::parse(&format!("http://{}", self.addr)).expect("a socket address is a valid host")
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().clone()
    }

    pub fn request_count(&self) -> usize {
        self.recorded.lock().len()
    }

    /// Wait until the stub has received `count` requests, failing the test after five seconds.
    pub async fn wait_for_requests(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while self.request_count() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the stub received {} of {count} requests",
                self.request_count()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// Serve one connection, request after request, so a pooled client is handled the way a real service
/// would handle it.
async fn serve(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    script: Arc<Mutex<Vec<StubResponse>>>,
    served: Arc<Mutex<usize>>,
    mode: Mode,
) -> std::io::Result<()> {
    let mut buffer = Vec::with_capacity(1024);

    loop {
        // Read until the head is complete.
        let head_end = loop {
            if let Some(position) = find_head_end(&buffer) {
                break position;
            }

            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..read]);
        };

        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let target = parts.next().unwrap_or_default().to_owned();

        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
        }

        let content_length: usize = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);

        // Read the body.
        let body_start = head_end + 4;
        while buffer.len() < body_start + content_length {
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = buffer[body_start..body_start + content_length].to_vec();

        let request = RecordedRequest {
            method,
            target,
            headers,
            body,
        };
        let built = match &mode {
            Mode::Responding(respond) => Some(respond(&request)),
            Mode::Scripted | Mode::Silent => None,
        };
        recorded.lock().push(request);

        buffer.drain(..body_start + content_length);

        if matches!(mode, Mode::Silent) {
            // Hold the connection open with no answer, until the client gives up and drops it.
            std::future::pending::<()>().await;
        }

        let response = built.unwrap_or_else(|| {
            let script = script.lock();
            let mut served = served.lock();
            let index = (*served).min(script.len().saturating_sub(1));
            *served += 1;

            script
                .get(index)
                .cloned()
                .unwrap_or_else(|| StubResponse::ok("{}"))
        });

        if response.hangup {
            return Ok(());
        }

        let mut head = match response.chunked {
            Some(_) => format!(
                "HTTP/1.1 {} STUB\r\nTransfer-Encoding: chunked\r\n",
                response.status
            ),
            None => format!(
                "HTTP/1.1 {} STUB\r\nContent-Length: {}\r\n",
                response.status,
                response.body.len()
            ),
        };
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes()).await?;

        match response.chunked {
            Some(chunk) => {
                for piece in response.body.chunks(chunk.max(1)) {
                    if let Some(pause) = response.pause {
                        tokio::time::sleep(pause).await;
                    }
                    stream
                        .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
                        .await?;
                    stream.write_all(piece).await?;
                    stream.write_all(b"\r\n").await?;
                }
                stream.write_all(b"0\r\n\r\n").await?;
            }
            None => stream.write_all(&response.body).await?,
        }

        stream.flush().await?;
    }
}

/// Where the head ends, if the buffer holds all of it.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}
