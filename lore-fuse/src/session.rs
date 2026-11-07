// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The session: negotiates INIT on worker 0, then runs one request loop per configured worker
//! (spec: "Request loop"), each decoding, dispatching, replying to, and recording a metrics
//! sample for every request in turn.
//!
//! Deviates from the plan's literal `Session`/`worker_loop` interface text in two load-bearing
//! ways:
//!
//! - `negotiated` is a plain, always-initialized [`Negotiated`], not a `OnceLock<Negotiated>`.
//!   The INIT handshake (`negotiate_init`) runs to completion *before* `Session` is
//!   constructed at all, so there is never a window where [`Session::negotiated`] would need to
//!   unwrap an empty `OnceLock` -- which the repo's "never `unwrap()`/`expect()`, or code that
//!   can panic, in production code" rule (`docs/developing/code-standards/errors.md`) forbids
//!   doing safely from a `&self -> &Negotiated` accessor.
//! - a `deferred: Arc<DeferredInterrupts>` field (and matching `worker_loop` parameter) that the
//!   plan's interface text omits: `dispatch` hard-requires one, so the omission is treated as
//!   a plan/code mismatch rather than an intentional narrowing.
use std::num::NonZeroUsize;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zerocopy::FromBytes;

use crate::abi::InHeader;
use crate::abi::Opcode;
use crate::buffer::ReplyBuffer;
use crate::buffer::RequestBuffer;
use crate::codec::reply::ReplyHeaderBuf;
use crate::codec::reply::encode_error;
use crate::codec::reply::encode_struct;
use crate::codec::request::DecodeError;
use crate::codec::request::Request;
use crate::codec::request::decode;
use crate::dispatch::DeferredInterrupts;
use crate::dispatch::dispatch;
use crate::error::FuseError;
use crate::filesystem::Filesystem;
use crate::init::InitConfig;
use crate::init::Negotiated;
use crate::init::negotiate;
use crate::interrupt::InFlightSlots;
use crate::metrics::Metrics;
use crate::reply::Errno;
use crate::transport::Notifier;
use crate::transport::Transport;
use crate::transport::TransportError;
use crate::transport::TransportWorker;

/// How many workers a [`Session`] runs, and how it negotiates INIT.
pub struct SessionConfig {
    pub workers: usize,
    pub init: InitConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        let available = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        Self {
            workers: available.min(8),
            init: InitConfig::default(),
        }
    }
}

/// A running FUSE session: one INIT-negotiated view of the kernel's capabilities, shared by
/// every worker's request loop, plus the shared state those loops coordinate through
/// (metrics, in-flight slots, deferred-read interrupts) and the handles needed to shut them
/// down.
pub struct Session<F: Filesystem, T: Transport> {
    fs: Arc<F>,
    transport: Arc<T>,
    negotiated: Negotiated,
    metrics: Arc<Metrics>,
    slots: Arc<InFlightSlots>,
    deferred: Arc<DeferredInterrupts>,
    shutdown: CancellationToken,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl<F, T> Session<F, T>
where
    F: Filesystem,
    T: Transport,
{
    /// Negotiates INIT on a first worker (spec: "worker 0 serves INIT"), then starts every
    /// worker's request loop, including that same first worker's. A worker that fails to open
    /// stops and joins the workers already started before the error returns.
    pub async fn start(
        fs: Arc<F>,
        transport: Arc<T>,
        config: SessionConfig,
    ) -> Result<Arc<Self>, FuseError> {
        let worker_count = config.workers.max(1);

        let mut worker0 = transport.worker()?;
        let negotiated = negotiate_init(&mut worker0, &config.init).await?;

        let session = Arc::new(Self {
            fs,
            transport,
            negotiated,
            metrics: Arc::new(Metrics::new()),
            slots: Arc::new(InFlightSlots::new(worker_count)),
            deferred: Arc::new(DeferredInterrupts::new()),
            shutdown: CancellationToken::new(),
            workers: Mutex::new(Vec::new()),
        });

        let mut handles = Vec::with_capacity(worker_count);
        handles.push(spawn_worker(Arc::clone(&session), 0, worker0));
        for index in 1..worker_count {
            match session.transport.worker() {
                Ok(worker) => handles.push(spawn_worker(Arc::clone(&session), index, worker)),
                Err(error) => {
                    session.shutdown.cancel();
                    for handle in handles {
                        let _ = handle.await;
                    }
                    return Err(error);
                }
            }
        }
        *session.workers.lock() = handles;

        Ok(session)
    }

    /// The protocol capabilities negotiated with the kernel at startup.
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// This session's per-opcode metrics.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// How many requests this session answered off their worker.
    pub fn deferred_requests(&self) -> u64 {
        self.deferred.deferred_total()
    }

    /// A handle for sending unsolicited notifications, independent of any worker.
    pub fn notifier(&self) -> Result<Notifier, FuseError> {
        self.transport.notifier()
    }

    /// Cancels every worker's request loop and waits for them all to stop.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        self.wait().await;
    }

    /// Waits for every worker to stop, however it stopped (an explicit `shutdown`, or the
    /// transport ending on its own, e.g. the kernel unmounted the filesystem).
    pub async fn wait(&self) {
        let handles = std::mem::take(&mut *self.workers.lock());
        for handle in handles {
            let _ = handle.await;
        }
    }
}

/// Spawns one worker's request loop as a detached task, following whatever runtime is already
/// current (`lore_base::lore_spawn!`), rather than a bespoke `tokio::spawn`.
fn spawn_worker<F, T>(
    session: Arc<Session<F, T>>,
    index: usize,
    worker: T::Worker,
) -> JoinHandle<()>
where
    F: Filesystem,
    T: Transport,
{
    lore_base::lore_spawn!(async move {
        let fs = Arc::clone(&session.fs);
        let metrics = Arc::clone(&session.metrics);
        let slots = Arc::clone(&session.slots);
        let deferred = Arc::clone(&session.deferred);
        let shutdown = session.shutdown.clone();
        let negotiated = session.negotiated;
        worker_loop::<F, T>(
            index,
            fs,
            worker,
            &negotiated,
            metrics,
            slots,
            deferred,
            shutdown,
        )
        .await;
    })
}

/// One worker's request loop (spec: "Request loop"): decode, dispatch, reply, and record --
/// until `shutdown` fires or the transport ends. Both `request_buffer`/`reply_scratch` are
/// allocated once, before the loop starts, and reused for every request this worker ever
/// serves (`buffer.rs`'s documented intent), which is what makes the inline path allocation-free
/// at this layer; [`dispatch`] itself never allocates on that path either.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn worker_loop<F, T>(
    index: usize,
    fs: Arc<F>,
    mut worker: T::Worker,
    negotiated: &Negotiated,
    metrics: Arc<Metrics>,
    slots: Arc<InFlightSlots>,
    deferred: Arc<DeferredInterrupts>,
    shutdown: CancellationToken,
) where
    F: Filesystem,
    T: Transport,
{
    let mut request_buffer = RequestBuffer::for_max_write(negotiated.max_write);
    let mut reply_scratch = ReplyBuffer::new(negotiated.max_pages);

    loop {
        let received = tokio::select! {
            result = worker.receive(&mut request_buffer) => result,
            _ = shutdown.cancelled() => return,
        };

        let bytes = match received {
            Ok(bytes) => bytes,
            // The transport is gone (kernel disconnected, or a real I/O error); nothing left
            // for this worker to do.
            Err(_) => return,
        };

        let data = request_buffer.as_mut();
        let decoded = match decode(&data[..bytes]) {
            Ok(decoded) => decoded,
            Err(error) => {
                if answer_undecodable(&mut worker, &data[..bytes], &error).is_err() {
                    return;
                }
                continue;
            }
        };

        let result = dispatch(
            &fs,
            &mut worker,
            index,
            decoded.header,
            decoded.request,
            &metrics,
            &slots,
            &deferred,
            reply_scratch.as_mut(),
        )
        .await;

        if result.is_err() {
            return;
        }
    }
}

/// Answers a request whose header parses but whose body does not, so its caller is not left
/// waiting: `ENAMETOOLONG` for a name that does not decode, `EIO` for any other body. A buffer
/// too short for a header names no request, and `FORGET` and `BATCH_FORGET` expect no reply.
fn answer_undecodable<W: TransportWorker>(
    worker: &mut W,
    buffer: &[u8],
    error: &DecodeError,
) -> Result<(), TransportError> {
    let Ok((header, _)) = InHeader::ref_from_prefix(buffer) else {
        return Ok(());
    };
    if matches!(
        Opcode::try_from(header.opcode),
        Ok(Opcode::Forget | Opcode::BatchForget)
    ) {
        return Ok(());
    }
    let errno = match error {
        DecodeError::BadName(_) => Errno::ENAMETOOLONG,
        _ => Errno::EIO,
    };
    let mut reply = ReplyHeaderBuf::new();
    worker.send(&encode_error(header.unique, errno.0, &mut reply))?;
    Ok(())
}

/// Runs INIT to completion on `worker` (spec: "worker 0 serves INIT"), before that same worker
/// joins the ordinary [`worker_loop`]. `dispatch`/`dispatch_request` deliberately do not handle
/// `Request::Init` themselves (a second one reaching there replies `EIO`, see `dispatch.rs`):
/// this is the only place INIT is ever answered.
async fn negotiate_init<W: TransportWorker>(
    worker: &mut W,
    config: &InitConfig,
) -> Result<Negotiated, FuseError> {
    let mut buffer = RequestBuffer::new();
    let bytes = worker
        .receive(&mut buffer)
        .await
        .map_err(|error| FuseError::Internal(format!("transport error before INIT: {error}")))?;

    let data = buffer.as_mut();
    let decoded = decode(&data[..bytes])
        .map_err(|error| FuseError::Internal(format!("bad INIT request: {error}")))?;

    let init_in = match decoded.request {
        Request::Init(body) => body,
        other => {
            return Err(FuseError::Internal(format!(
                "expected INIT as the first request, got {other:?}"
            )));
        }
    };

    let (negotiated, init_out) = negotiate(init_in, config)?;

    let mut reply_buf = ReplyHeaderBuf::new();
    let iov = encode_struct(decoded.header.unique, &init_out, &mut reply_buf);
    worker.send(&iov).map_err(|error| {
        FuseError::Internal(format!("transport error replying to INIT: {error}"))
    })?;

    Ok(negotiated)
}
