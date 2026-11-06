// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
use lore_base::error::Disconnected;
use lore_base::error::NotFound;
use lore_base::error::SlowDown;
use lore_base::lore_debug;
use lore_base::lore_error;
use lore_base::lore_spawn_net;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_base::types::VerifyResult;
use lore_error_set::prelude::*;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_proto::lore::storage::v1::storage_service_client::StorageServiceClient;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tonic::metadata::MetadataValue;

use super::CORRELATION_ID_HEADER;
use super::PARTITION_ID_KEY;
use super::REPOSITORY_ID_KEY;
use crate::error::ProtocolError;

/// Translate a response's in-band `status` into a [`ProtocolError`], or `None` when the item
/// succeeded. Absence means `OK`, so a peer that predates the field reads as success.
fn item_status_error(
    status: Option<&lore_proto::lore::model::v1::ItemStatus>,
) -> Option<ProtocolError> {
    let status = status?;
    let code = tonic::Code::from_i32(status.code as i32);
    if code == tonic::Code::Ok {
        return None;
    }
    Some(ProtocolError::from(tonic::Status::new(
        code,
        status.message.clone(),
    )))
}

const STREAM_WRITE_BUFFER_SIZE: usize = 32 * 1024;
const INFLIGHT_COMMAND_LIMIT: usize = 10000;

/// Bound on stream-level reissues before handing off to connection-level reconnect.
///
/// Reissuing here only covers a stream dying on an otherwise healthy channel, which needs no
/// backoff. Anything the channel itself is responsible for belongs to
/// `GRPCConnection::reconnect`, which owns the epoch, single-flight guard and backoff — so
/// this only has to be large enough to absorb a server resetting individual streams.
const MAX_STREAM_REISSUES: usize = 8;

/// Session context for gRPC metadata injection. Cached at `session_start` time.
#[derive(Clone)]
pub struct GrpcSessionContext {
    pub partition: Partition,
    pub correlation_id: String,
    pub auth_token: String,
}

/// Which streaming RPC a cached stream belongs to.
///
/// Part of the stream-cache key so rotating a dead Get stream leaves the session's
/// `GetMetadata`, Put and Copy streams untouched — they're independent RPCs and one dying
/// says nothing about the others.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Verb {
    Get,
    GetMetadata,
    Put,
    Copy,
    GetResolved,
    PutResolved,
}

impl Verb {
    fn label(self) -> &'static str {
        match self {
            Verb::Get => "get",
            Verb::GetMetadata => "get_metadata",
            Verb::Put => "put",
            Verb::Copy => "copy",
            Verb::GetResolved => "get_resolved",
            Verb::PutResolved => "put_resolved",
        }
    }
}

type StreamItem<K, S> = (K, oneshot::Sender<Result<S, ProtocolError>>);

/// A live stream's request channel plus whether its RPC ever opened.
///
/// `opened` is what lets `StreamCache::request` tell a stream that died mid-flight from one
/// that never established, since the sender is handed out before the RPC is attempted. By
/// the time a request observes a failure the flag has settled: either the reader task
/// answered it, or the task exited and dropped the channel.
struct StreamState<K, S> {
    sender: mpsc::Sender<StreamItem<K, S>>,
    opened: AtomicBool,
}

type StreamHandle<K, S> = Arc<StreamState<K, S>>;

/// Keyed by the address a request reads or writes: the address the server echoes back, or for a
/// copy the target, see [`CopyRouting`]. A `Vec` per key so concurrent requests for the same
/// address coalesce onto one round trip and all get woken by its response.
type Inflight<S> = DashMap<Address, Vec<oneshot::Sender<Result<S, ProtocolError>>>>;

/// Returns true for the first waiter on `key`, meaning the caller should put the request on
/// the wire; later waiters ride along on that one round trip.
fn register<S>(
    inflight: &Inflight<S>,
    key: Address,
    sender: oneshot::Sender<Result<S, ProtocolError>>,
) -> bool {
    let mut first = false;
    #[allow(clippy::disallowed_methods)] // Brief write lock; no await while held.
    inflight
        .entry(key)
        .or_insert_with(|| {
            first = true;
            Vec::new()
        })
        .push(sender);
    first
}

/// Answering explicitly rather than letting the senders drop: a dropped sender reaches the
/// caller as an opaque `RecvError` that the storage layer can only classify as an internal
/// fault, whereas a real error tells it whether a retry is worth attempting.
fn fail_inflight<S>(inflight: &Inflight<S>, verb: Verb, err: &ProtocolError) {
    let mut failed = 0usize;
    inflight.retain(|_, senders| {
        for sender in senders.drain(..) {
            failed += 1;
            let _ = sender.send(Err(err.clone()));
        }
        false
    });
    if failed > 0 {
        lore_debug!(
            "{} stream ended with {failed} request(s) outstanding: {err}",
            verb.label()
        );
    }
}

/// Lets one generic pump serve every streaming verb: the only per-verb differences are which
/// field carries the routing address and where an in-band per-item failure lives.
trait StreamResponse: Sized {
    type Success: Clone + Send + 'static;

    /// The key this response's request was registered under.
    fn key(&self) -> Option<Address>;

    /// Preserves the server's status code, which callers pattern-match on — a remote
    /// `NotFound` is a routine answer on the metadata path rather than a fault.
    ///
    /// An absent status is success: the field postdates the original response, so a peer
    /// without it signals a per-item failure by ending the stream instead.
    fn into_result(self) -> Result<Self::Success, ProtocolError>;
}

impl StreamResponse for storage_v1::GetResponse {
    type Success = (model_v1::Fragment, Bytes);

    fn key(&self) -> Option<Address> {
        self.address.as_ref().map(Address::from)
    }

    fn into_result(self) -> Result<Self::Success, ProtocolError> {
        if let Some(status) = self.status.as_ref().filter(|status| !status.is_ok()) {
            return Err(ProtocolError::from(tonic::Status::from(status)));
        }
        let fragment = self
            .fragment
            .ok_or_else(|| ProtocolError::internal("get: successful response has no fragment"))?;
        Ok((fragment, self.payload))
    }
}

impl StreamResponse for storage_v1::PutResponse {
    type Success = ();

    fn key(&self) -> Option<Address> {
        self.address.as_ref().map(Address::from)
    }

    fn into_result(self) -> Result<Self::Success, ProtocolError> {
        match self.status.as_ref().filter(|status| !status.is_ok()) {
            Some(status) => Err(ProtocolError::from(tonic::Status::from(status))),
            None => Ok(()),
        }
    }
}

impl StreamResponse for storage_v1::CopyResponse {
    type Success = ();

    fn key(&self) -> Option<Address> {
        self.source_address.as_ref().map(Address::from)
    }

    fn into_result(self) -> Result<Self::Success, ProtocolError> {
        match self.status.as_ref().filter(|status| !status.is_ok()) {
            Some(status) => Err(ProtocolError::from(tonic::Status::from(status))),
            None => Ok(()),
        }
    }
}

/// The copies on a copy stream, by source address. A copy response names only its source, so a
/// source address is on the wire for one target at a time, and a copy of it to another target
/// waits for the response to the one before.
#[derive(Default)]
struct CopyRouting {
    /// The target each source address on the wire is being copied to.
    wire: HashMap<Address, Address>,
    /// The copies waiting for a source address on the wire, in arrival order.
    queued: HashMap<Address, VecDeque<storage_v1::CopyRequest>>,
}

impl CopyRouting {
    /// The source and target addresses of `request`. The target is in the session's partition,
    /// with the source's hash and the target context, or the source's context when it names
    /// none.
    fn addresses(request: &storage_v1::CopyRequest) -> Option<(Address, Address)> {
        let source = Address::from(request.source_address.as_ref()?);
        let context = match request.target_context.as_ref() {
            [] => source.context,
            bytes => Context::from(<[u8; 16]>::try_from(bytes).ok()?),
        };
        Some((
            source,
            Address {
                hash: source.hash,
                context,
            },
        ))
    }

    /// Put `request` on the wire for `source`, returning it, or queue it behind the copy of
    /// `source` on the wire.
    fn admit(
        &mut self,
        source: Address,
        target: Address,
        request: storage_v1::CopyRequest,
    ) -> Option<storage_v1::CopyRequest> {
        match self.wire.entry(source) {
            Entry::Occupied(_) => {
                self.queued.entry(source).or_default().push_back(request);
                None
            }
            Entry::Vacant(wire) => {
                wire.insert(target);
                Some(request)
            }
        }
    }

    /// Take the answered copy of `source` off the wire, returning its target and the next copy
    /// of `source`, which goes on the wire in its place.
    fn answered(&mut self, source: Address) -> (Option<Address>, Option<storage_v1::CopyRequest>) {
        let target = self.wire.remove(&source);
        let Some(queue) = self.queued.get_mut(&source) else {
            return (target, None);
        };
        let next = queue.pop_front();
        if queue.is_empty() {
            self.queued.remove(&source);
        }
        if let Some((_, next_target)) = next.as_ref().and_then(Self::addresses) {
            self.wire.insert(source, next_target);
        }
        (target, next)
    }
}

/// A per-item failure arrives in-band on an `Ok` message and costs only that request. An
/// `Err` is terminal by construction — tonic surfaces a stream status once and then reports
/// the stream exhausted — so it ends the loop and everything still outstanding fails.
///
/// Those outstanding requests are failed as `Disconnected` whatever the terminal code says,
/// because a stream status describes the stream rather than any one request on it. That is
/// also what lets `StreamCache::request` replay them: the ordinary ways a connection dies
/// arrive as `Internal` or `Cancelled` (`Status::from_h2_error`), neither of which a caller
/// would otherwise retry. The real status is kept on the error's trace.
///
/// `route` maps the address a response echoes to the key its request was registered under.
async fn pump_responses<R>(
    mut stream: tonic::Streaming<R>,
    inflight: Arc<Inflight<R::Success>>,
    verb: Verb,
    mut route: impl FnMut(Address) -> Option<Address>,
) where
    R: StreamResponse,
{
    let mut terminal = ProtocolError::from(Disconnected);

    while let Some(message) = stream.next().await {
        let response = match message {
            Ok(response) => response,
            Err(status) => {
                let mut err = ProtocolError::from(Disconnected);
                err.push_trace(lore_error_set::Location::with_context(
                    file!(),
                    line!(),
                    column!(),
                    Arc::from(format!("{} stream terminated: {status}", verb.label())),
                ));
                terminal = err;
                break;
            }
        };

        let Some(address) = response.key() else {
            lore_error!("{} response missing address", verb.label());
            continue;
        };
        let Some((_, senders)) = route(address).and_then(|key| inflight.remove(&key)) else {
            lore_error!(
                "{} received unexpected result for address {address}",
                verb.label()
            );
            continue;
        };

        let result = response.into_result();
        let mut senders = senders;
        if let Some(last) = senders.pop() {
            for sender in senders {
                let _ = sender.send(result.clone());
            }
            let _ = last.send(result);
        }
    }

    fail_inflight(&inflight, verb, &terminal);
}

/// The hot path is a single `DashMap` read plus an `Arc` clone — no write lock and no second
/// level of indirection. A dead stream announces itself by failing the send, so nothing polls
/// for liveness and the reader tasks never touch this map: the request path is its only
/// mutator, which is what makes rotation race-free.
struct StreamCache<K, S> {
    streams: DashMap<(u32, Verb), StreamHandle<K, S>>,
}

impl<K: Clone, S> StreamCache<K, S> {
    fn new() -> Self {
        Self {
            streams: DashMap::new(),
        }
    }

    /// Dropping the last sender ends the request generator, which ends the RPC and its
    /// reader task.
    fn remove(&self, session_id: u32, verb: Verb) {
        self.streams.remove(&(session_id, verb));
    }

    /// Drop a handle whose stream never opened, so the next request establishes a fresh one
    /// instead of inheriting a known-dead entry and giving up on it again. Pointer identity keeps
    /// a replacement installed by a concurrent caller safe.
    fn discard(&self, key: (u32, Verb), failed: &StreamHandle<K, S>) {
        #[allow(clippy::disallowed_methods)] // Brief write lock; no await while held.
        self.streams
            .remove_if(&key, |_, current| Arc::ptr_eq(current, failed));
    }

    /// `failed` is the handle whose send just failed. Pointer identity against the current
    /// entry is the single-flight check, and doing it under the shard lock makes it exact: of N
    /// callers that all failed on the same dead handle, the first to take the lock spawns and
    /// the rest adopt its replacement, so `spawn` runs once per stream death, not per caller.
    fn rotate(
        &self,
        key: (u32, Verb),
        failed: Option<&StreamHandle<K, S>>,
        spawn: impl FnOnce() -> StreamHandle<K, S>,
    ) -> StreamHandle<K, S> {
        use dashmap::mapref::entry::Entry;

        #[allow(clippy::disallowed_methods)] // Cold path; `spawn` does not await.
        match self.streams.entry(key) {
            Entry::Occupied(mut occupied) => {
                let superseded = match failed {
                    Some(failed) => !Arc::ptr_eq(occupied.get(), failed),
                    // Nothing of ours to compare against, so any cached handle wins.
                    None => true,
                };
                if superseded {
                    return occupied.get().clone();
                }
                let fresh = spawn();
                occupied.insert(fresh.clone());
                fresh
            }
            Entry::Vacant(vacant) => {
                let fresh = spawn();
                vacant.insert(fresh.clone());
                fresh
            }
        }
    }

    /// Reconnect and reissue on a stream death, the way the QUIC client's
    /// `send_with_reconnect` does, so a caller never sees one.
    ///
    /// A death shows up three ways, all handled here: reserving a place in the queue fails
    /// because the receiver is already gone (nothing has been sent, so nothing is lost), the
    /// reader answers with a disconnect, or it exits without answering at all. Anything else the
    /// reader answers is the server's verdict on this request and goes straight back —
    /// matching QUIC, where `NotFound`, `SlowDown` and `NotAuthorized` bubble rather than
    /// provoking a reconnect.
    ///
    /// Reissuing continues while the remote is still reachable. A replacement stream that will
    /// not open means the channel is suspect rather than the stream, so it hands straight back
    /// as `Disconnected` for `GRPCStorage` to drive `GRPCConnection::reconnect` — the same
    /// division QUIC draws between reissuing a command and reconnecting the socket. The dead
    /// handle is discarded on the way out, so the request that follows a successful reconnect
    /// establishes a stream on the new channel rather than inheriting this one.
    async fn request(
        &self,
        key: (u32, Verb),
        payload: K,
        spawn: impl Fn() -> StreamHandle<K, S>,
    ) -> Result<S, ProtocolError> {
        let mut failed: Option<StreamHandle<K, S>> = None;

        for _ in 0..MAX_STREAM_REISSUES {
            let handle = match (&failed, self.streams.get(&key)) {
                (None, Some(entry)) => entry.value().clone(),
                (_, entry) => {
                    // `rotate` write-locks the shard this read guard holds, so keeping the
                    // guard here self-deadlocks.
                    drop(entry);
                    self.rotate(key, failed.as_ref(), &spawn)
                }
            };

            let answer = match handle.sender.reserve().await {
                Ok(permit) => {
                    let (tx, rx) = oneshot::channel();
                    permit.send((payload.clone(), tx));
                    rx.await.ok()
                }
                Err(_) => None,
            };
            let server_verdict =
                answer.filter(|answer| !matches!(answer, Err(err) if err.is_disconnected()));
            if let Some(result) = server_verdict {
                return result;
            }

            if !handle.opened.load(Ordering::Relaxed) {
                self.discard(key, &handle);
                return Err(ProtocolError::from(Disconnected));
            }
            failed = Some(handle);
        }

        Err(ProtocolError::from(Disconnected))
    }
}

pub struct StorageService {
    /// Resolved to a client per stream open rather than cached, so a channel rebuilt by
    /// `GRPCConnection::reconnect` is picked up by the next rotation without costing the
    /// request path a lock.
    connection: Arc<super::GRPCConnection>,
    /// Get and `GetMetadata` share a cache: same request and response shape, told apart by
    /// `Verb`.
    get_streams: StreamCache<Address, (model_v1::Fragment, Bytes)>,
    put_streams: StreamCache<storage_v1::PutRequest, ()>,
    copy_streams: StreamCache<storage_v1::CopyRequest, ()>,
    /// Correlates resolved requests with their responses; never handed out as zero, which the
    /// server treats as uncorrelatable and stream-fatal.
    resolved_counter: AtomicU64,
    /// The resolved verbs correlate by `request_id`, not by address: one key can resolve to any
    /// content, so the address is an answer rather than a question. They carry their own reader
    /// instead of `pump_responses`.
    get_resolved_streams:
        StreamCache<storage_v1::GetResolvedRequest, Arc<storage_v1::GetResolvedResponse>>,
    put_resolved_streams:
        StreamCache<storage_v1::PutResolvedRequest, Arc<storage_v1::PutResolvedResponse>>,
    get_put_limiter: Semaphore,
}

fn inject_metadata<T>(request: &mut tonic::Request<T>, ctx: &GrpcSessionContext) {
    let md = request.metadata_mut();
    md.insert_bin(
        PARTITION_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(ctx.partition.data()),
    );
    md.insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(ctx.partition.data()),
    );
    if !ctx.correlation_id.is_empty()
        && let Ok(val) = MetadataValue::from_str(&ctx.correlation_id)
    {
        md.insert(CORRELATION_ID_HEADER, val);
    }
    if !ctx.auth_token.is_empty()
        && let Ok(mut val) = MetadataValue::from_str(&format!("Bearer {}", ctx.auth_token))
    {
        val.set_sensitive(true);
        md.insert("authorization", val);
    }
}

/// `status`, the failure of the unary `request`, as a protocol error. `Unavailable` reads as
/// `SlowDown`, a server too loaded to take the request, which the general conversion reads as a
/// lost connection. A refusal of the credentials keeps its code, and any other failure is internal.
fn unary_error(status: tonic::Status, request: &'static str) -> ProtocolError {
    match status.code() {
        tonic::Code::Unavailable => ProtocolError::from(SlowDown),
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => ProtocolError::from(status),
        tonic::Code::Unimplemented => ProtocolError::internal(format!("unsupported: {request}")),
        _ => ProtocolError::internal_with_context(status, request),
    }
}

impl StorageService {
    pub fn new(connection: Arc<super::GRPCConnection>) -> Self {
        Self {
            connection,
            get_streams: StreamCache::new(),
            put_streams: StreamCache::new(),
            copy_streams: StreamCache::new(),
            resolved_counter: AtomicU64::new(0),
            get_resolved_streams: StreamCache::new(),
            put_resolved_streams: StreamCache::new(),
            get_put_limiter: Semaphore::new(INFLIGHT_COMMAND_LIMIT),
        }
    }

    /// Remove streams for a session. Dropping the senders terminates the stream tasks.
    pub fn remove_session_streams(&self, session_id: u32) {
        self.get_streams.remove(session_id, Verb::Get);
        self.get_streams.remove(session_id, Verb::GetMetadata);
        self.put_streams.remove(session_id, Verb::Put);
        self.copy_streams.remove(session_id, Verb::Copy);
        self.get_resolved_streams
            .remove(session_id, Verb::GetResolved);
        self.put_resolved_streams
            .remove(session_id, Verb::PutResolved);
    }

    pub async fn get(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        lore_debug!("gRPC get fragment: {}", address);

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;

        let (fragment, payload) = self
            .get_streams
            .request((session_id, Verb::Get), *address, || {
                self.spawn_get_stream(ctx)
            })
            .await?;

        let fragment = Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        };

        if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
            lore_error!("Invalid fragment in get response {fragment:?}: {reason}");
            return Err(ProtocolError::internal(format!(
                "get: invalid fragment: {reason}"
            )));
        }
        if payload.len() != fragment.size_payload as usize {
            lore_error!(
                "Fragment payload is invalid in get response : {} bytes, expected {}",
                payload.len(),
                fragment.size_payload
            );
            return Err(ProtocolError::internal("get: Invalid payload"));
        }

        Ok((fragment, payload))
    }

    /// Fetch only the fragment metadata for an address. Same wire request as `get` (just an
    /// `Address`), but the server's response carries `Fragment` only — no payload bytes — so
    /// callers that don't need the payload skip the transfer cost. Used by the storage API's
    /// query op for remote-hit metadata lookups.
    ///
    /// A `NotFound` here is an ordinary answer rather than a fault — see
    /// `RemoteImmutableStore::get_metadata`, which maps it to `MatchNone` — so it arrives
    /// in-band and costs only this lookup.
    pub async fn get_metadata(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        lore_debug!("gRPC get_metadata fragment: {}", address);

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;

        let (fragment, _payload) = self
            .get_streams
            .request((session_id, Verb::GetMetadata), *address, || {
                self.spawn_get_metadata_stream(ctx)
            })
            .await?;

        let fragment = Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        };

        if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
            lore_error!("Invalid fragment in get_metadata response {fragment:?}: {reason}");
            return Err(ProtocolError::internal(format!(
                "get_metadata: invalid fragment: {reason}"
            )));
        }

        Ok(fragment)
    }

    pub async fn put(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        lore_debug!("Put fragment: {address}");

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;

        let request = storage_v1::PutRequest {
            address: Some(address.into()),
            fragment: Some(fragment.into()),
            payload,
        };

        self.put_streams
            .request((session_id, Verb::Put), request, || {
                self.spawn_put_stream(ctx)
            })
            .await
    }

    pub async fn query(
        &self,
        ctx: &GrpcSessionContext,
        address: &[Address],
    ) -> Result<Bytes, ProtocolError> {
        lore_debug!("Query {} fragments", address.len());

        let request = storage_v1::QueryRequest {
            addresses: address.iter().map(model_v1::Address::from).collect(),
        };
        let mut client = StorageServiceClient::new(self.connection.channel());
        let mut req = tonic::Request::new(request);
        inject_metadata(&mut req, ctx);

        let res = client
            .query(req)
            .await
            .map(|res| res.into_inner())
            .map_err(|err| unary_error(err, "query"))?;

        let mut buffer = BytesMut::with_capacity(res.results.len());
        for value in res.results.iter() {
            buffer.put_u8(*value as u8);
        }
        Ok(buffer.freeze())
    }

    pub async fn verify(
        &self,
        ctx: &GrpcSessionContext,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        lore_debug!("Verify fragment: {address}");

        let request = storage_v1::VerifyRequest {
            address: Some((*address).into()),
            heal,
        };
        let mut client = StorageServiceClient::new(self.connection.channel());
        let mut req = tonic::Request::new(request);
        inject_metadata(&mut req, ctx);

        client
            .verify(req)
            .await
            .map(|res| res.into_inner())
            .map_err(|err| match err.code() {
                tonic::Code::NotFound => ProtocolError::from(NotFound),
                _ => unary_error(err, "verify"),
            })
            .map(|res| VerifyResult {
                corrupted: res.corrupted,
                healed: HealResult::from(res.healed),
            })
    }

    pub async fn copy(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        source_partition: Partition,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        lore_debug!(
            "gRPC copy fragment: {} from partition {} (target context {})",
            source_address,
            source_partition,
            target_context
        );

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;

        let request = storage_v1::CopyRequest {
            source_repository_id: Bytes::copy_from_slice(source_partition.data()),
            source_address: Some(source_address.into()),
            target_context: Bytes::copy_from_slice(zerocopy::IntoBytes::as_bytes(&target_context)),
        };

        self.copy_streams
            .request((session_id, Verb::Copy), request, || {
                self.spawn_copy_stream(ctx)
            })
            .await
    }

    pub async fn mutable_load(
        &self,
        ctx: &GrpcSessionContext,
        key: &Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        lore_debug!("gRPC mutable_load: {}", key);

        let request = storage_v1::MutableLoadRequest {
            key: Bytes::copy_from_slice(key.data()),
            key_type: key_type as u32,
        };
        let mut client = StorageServiceClient::new(self.connection.channel());
        let mut req = tonic::Request::new(request);
        inject_metadata(&mut req, ctx);

        let res = client
            .mutable_load(req)
            .await
            .map(|res| res.into_inner())
            .map_err(|err| match err.code() {
                tonic::Code::NotFound => ProtocolError::from(NotFound),
                _ => unary_error(err, "mutable_load"),
            })?;

        Ok(Hash::from(&res.value[..]))
    }

    pub async fn mutable_store(
        &self,
        ctx: &GrpcSessionContext,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        lore_debug!("gRPC mutable_store: {}", key);

        let request = storage_v1::MutableStoreRequest {
            key: Bytes::copy_from_slice(key.data()),
            value: Bytes::copy_from_slice(value.data()),
            key_type: key_type as u32,
        };
        let mut client = StorageServiceClient::new(self.connection.channel());
        let mut req = tonic::Request::new(request);
        inject_metadata(&mut req, ctx);

        client
            .mutable_store(req)
            .await
            .map(|_| ())
            .map_err(|err| unary_error(err, "mutable_store"))
    }

    pub async fn mutable_compare_and_swap(
        &self,
        ctx: &GrpcSessionContext,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        lore_debug!("gRPC mutable_cas: {}", key);

        let request = storage_v1::MutableCompareAndSwapRequest {
            key: Bytes::copy_from_slice(key.data()),
            expected: Bytes::copy_from_slice(expected.data()),
            value: Bytes::copy_from_slice(value.data()),
            key_type: key_type as u32,
        };
        let mut client = StorageServiceClient::new(self.connection.channel());
        let mut req = tonic::Request::new(request);
        inject_metadata(&mut req, ctx);

        let res = client
            .mutable_compare_and_swap(req)
            .await
            .map(|res| res.into_inner())
            .map_err(|err| unary_error(err, "mutable_cas"))?;

        Ok(Hash::from(&res.current_value[..]))
    }

    /// A failure to open the RPC keeps its real status rather than becoming a disconnect: it
    /// applies equally to everything queued behind it, and an `Unauthenticated` or
    /// `Unimplemented` there should reach the caller instead of being replayed.
    /// Reader for a resolved stream, correlating by `request_id`.
    ///
    /// `pump_responses` keys on the address the server echoes back, which the resolved verbs
    /// cannot use: the address is what the request is asking for, not what identifies it. The
    /// `opened` flag and the drain on exit follow the same contract as the address-keyed verbs,
    /// so a stream that dies mid-flight fails its waiters rather than leaving them parked.
    pub async fn get_resolved(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        key: &Hash,
        context: &Context,
        flags: u32,
    ) -> Result<(Hash, Fragment, Bytes), ProtocolError> {
        let key_address = Address {
            hash: *key,
            context: *context,
        };
        lore_debug!("gRPC get_resolved key: {}", key_address);

        let request_id = self
            .resolved_counter
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let request = storage_v1::GetResolvedRequest {
            request_id,
            key: Some(key_address.into()),
            flags,
        };

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;

        let res = self
            .get_resolved_streams
            .request((session_id, Verb::GetResolved), request, || {
                self.spawn_get_resolved_stream(ctx)
            })
            .await?;

        if res.resolved.len() != size_of::<Hash>() {
            lore_error!(
                "Invalid get_resolved response, resolved hash is {} bytes, expected {}",
                res.resolved.len(),
                size_of::<Hash>()
            );
            return Err(ProtocolError::internal(
                "get_resolved: Invalid resolved hash length",
            ));
        }

        let Some(fragment) = res.fragment else {
            lore_error!("Invalid get_resolved response, missing fragment");
            return Err(ProtocolError::internal("get_resolved: Missing fragment"));
        };

        let fragment = Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        };

        if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
            lore_error!("Invalid fragment in get_resolved response {fragment:?}: {reason}");
            return Err(ProtocolError::internal(format!(
                "get_resolved: invalid fragment: {reason}"
            )));
        }
        if res.payload.len() != fragment.size_payload as usize {
            lore_error!(
                "Fragment payload is invalid in get_resolved response: {} bytes, expected {}",
                res.payload.len(),
                fragment.size_payload
            );
            return Err(ProtocolError::internal("get_resolved: Invalid payload"));
        }

        Ok((Hash::from(&res.resolved[..]), fragment, res.payload.clone()))
    }

    pub async fn put_resolved(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        key: &Hash,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        lore_debug!("gRPC put_resolved key: {} -> {}", key, address);

        let request_id = self
            .resolved_counter
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let request = storage_v1::PutResolvedRequest {
            request_id,
            key: Bytes::from_owner(*key),
            address: Some(address.into()),
            fragment: Some(fragment.into()),
            payload: payload.unwrap_or_default(),
        };

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;

        self.put_resolved_streams
            .request((session_id, Verb::PutResolved), request, || {
                self.spawn_put_resolved_stream(ctx)
            })
            .await
            .map(|_| ())
    }

    fn spawn_get_resolved_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> StreamHandle<storage_v1::GetResolvedRequest, Arc<storage_v1::GetResolvedResponse>> {
        let mut client = StorageServiceClient::new(self.connection.channel());
        let (tx, mut rx) = mpsc::channel(STREAM_WRITE_BUFFER_SIZE);
        let handle = Arc::new(StreamState {
            sender: tx,
            opened: AtomicBool::new(false),
        });
        let state = handle.clone();
        let pending = Arc::new(DashMap::<
            u64,
            oneshot::Sender<Result<Arc<storage_v1::GetResolvedResponse>, ProtocolError>>,
        >::new());

        let request_pending = pending.clone();
        let requests = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let request: storage_v1::GetResolvedRequest = request;
                request_pending.insert(request.request_id, sender);
                yield request;
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(requests);
            inject_metadata(&mut req, &ctx);

            let drain = |err: ProtocolError| {
                let ids: Vec<u64> = pending.iter().map(|entry| *entry.key()).collect();
                for id in ids {
                    if let Some((_, sender)) = pending.remove(&id) {
                        let _ = sender.send(Err(err.clone()));
                    }
                }
            };

            let mut responses = match client.get_resolved(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    response.into_inner()
                }
                Err(status) => {
                    lore_debug!("{} request failed: {status}", Verb::GetResolved.label());
                    drain(ProtocolError::from(status));
                    return;
                }
            };

            while let Some(response) = responses.next().await {
                match response {
                    Ok(response) => {
                        let Some((_, sender)) = pending.remove(&response.request_id) else {
                            lore_error!(
                                "{} unexpected result for request_id {}",
                                Verb::GetResolved.label(),
                                response.request_id
                            );
                            continue;
                        };
                        let result = match item_status_error(response.status.as_ref()) {
                            Some(err) => Err(err),
                            None => Ok(Arc::new(response)),
                        };
                        let _ = sender.send(result);
                    }
                    Err(status) => {
                        drain(ProtocolError::from(status));
                        return;
                    }
                }
            }

            drain(ProtocolError::internal(
                "get_resolved: stream closed before responding",
            ));
        });

        handle
    }

    /// See [`StorageService::spawn_get_resolved_stream`]; the write side, same correlation.
    fn spawn_put_resolved_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> StreamHandle<storage_v1::PutResolvedRequest, Arc<storage_v1::PutResolvedResponse>> {
        let mut client = StorageServiceClient::new(self.connection.channel());
        let (tx, mut rx) = mpsc::channel(STREAM_WRITE_BUFFER_SIZE);
        let handle = Arc::new(StreamState {
            sender: tx,
            opened: AtomicBool::new(false),
        });
        let state = handle.clone();
        let pending = Arc::new(DashMap::<
            u64,
            oneshot::Sender<Result<Arc<storage_v1::PutResolvedResponse>, ProtocolError>>,
        >::new());

        let request_pending = pending.clone();
        let requests = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let request: storage_v1::PutResolvedRequest = request;
                request_pending.insert(request.request_id, sender);
                yield request;
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(requests);
            inject_metadata(&mut req, &ctx);

            let drain = |err: ProtocolError| {
                let ids: Vec<u64> = pending.iter().map(|entry| *entry.key()).collect();
                for id in ids {
                    if let Some((_, sender)) = pending.remove(&id) {
                        let _ = sender.send(Err(err.clone()));
                    }
                }
            };

            let mut responses = match client.put_resolved(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    response.into_inner()
                }
                Err(status) => {
                    lore_debug!("{} request failed: {status}", Verb::PutResolved.label());
                    drain(ProtocolError::from(status));
                    return;
                }
            };

            while let Some(response) = responses.next().await {
                match response {
                    Ok(response) => {
                        let Some((_, sender)) = pending.remove(&response.request_id) else {
                            lore_error!(
                                "{} unexpected result for request_id {}",
                                Verb::PutResolved.label(),
                                response.request_id
                            );
                            continue;
                        };
                        let result = match item_status_error(response.status.as_ref()) {
                            Some(err) => Err(err),
                            None => Ok(Arc::new(response)),
                        };
                        let _ = sender.send(result);
                    }
                    Err(status) => {
                        drain(ProtocolError::from(status));
                        return;
                    }
                }
            }

            drain(ProtocolError::internal(
                "put_resolved: stream closed before responding",
            ));
        });

        handle
    }

    fn spawn_get_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> StreamHandle<Address, (model_v1::Fragment, Bytes)> {
        let mut client = StorageServiceClient::new(self.connection.channel());
        let (tx, mut rx) = mpsc::channel(STREAM_WRITE_BUFFER_SIZE);
        let handle = Arc::new(StreamState {
            sender: tx,
            opened: AtomicBool::new(false),
        });
        let state = handle.clone();
        let inflight: Arc<Inflight<(model_v1::Fragment, Bytes)>> = Arc::new(DashMap::new());

        let request_inflight = inflight.clone();
        let requests = async_stream::stream! {
            while let Some((address, sender)) = rx.recv().await {
                if register(&request_inflight, address, sender) {
                    yield model_v1::Address::from(address);
                }
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(requests);
            inject_metadata(&mut req, &ctx);

            match client.get(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    pump_responses(response.into_inner(), inflight, Verb::Get, Some).await;
                }
                Err(status) => {
                    lore_debug!("{} request failed: {status}", Verb::Get.label());
                    fail_inflight(&inflight, Verb::Get, &ProtocolError::from(status));
                }
            }
        });

        handle
    }

    /// A failure to open the RPC keeps its real status rather than becoming a disconnect: it
    /// applies equally to everything queued behind it, and an `Unauthenticated` or
    /// `Unimplemented` there should reach the caller instead of being replayed.
    fn spawn_get_metadata_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> StreamHandle<Address, (model_v1::Fragment, Bytes)> {
        let mut client = StorageServiceClient::new(self.connection.channel());
        let (tx, mut rx) = mpsc::channel(STREAM_WRITE_BUFFER_SIZE);
        let handle = Arc::new(StreamState {
            sender: tx,
            opened: AtomicBool::new(false),
        });
        let state = handle.clone();
        let inflight: Arc<Inflight<(model_v1::Fragment, Bytes)>> = Arc::new(DashMap::new());

        let request_inflight = inflight.clone();
        let requests = async_stream::stream! {
            while let Some((address, sender)) = rx.recv().await {
                if register(&request_inflight, address, sender) {
                    yield model_v1::Address::from(address);
                }
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(requests);
            inject_metadata(&mut req, &ctx);

            match client.get_metadata(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    pump_responses(response.into_inner(), inflight, Verb::GetMetadata, Some).await;
                }
                Err(status) => {
                    lore_debug!("{} request failed: {status}", Verb::GetMetadata.label());
                    fail_inflight(&inflight, Verb::GetMetadata, &ProtocolError::from(status));
                }
            }
        });

        handle
    }

    fn spawn_put_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> StreamHandle<storage_v1::PutRequest, ()> {
        let mut client = StorageServiceClient::new(self.connection.channel());
        let (tx, mut rx) =
            mpsc::channel::<StreamItem<storage_v1::PutRequest, ()>>(STREAM_WRITE_BUFFER_SIZE);
        let handle = Arc::new(StreamState {
            sender: tx,
            opened: AtomicBool::new(false),
        });
        let state = handle.clone();
        let inflight: Arc<Inflight<()>> = Arc::new(DashMap::new());

        let request_inflight = inflight.clone();
        let requests = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let Some(address) = request.address.as_ref().map(Address::from) else {
                    lore_debug!("Missing address in put request");
                    let _ = sender.send(Err(ProtocolError::internal("put: missing address")));
                    continue;
                };
                if register(&request_inflight, address, sender) {
                    yield request;
                }
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(requests);
            inject_metadata(&mut req, &ctx);

            match client.put(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    pump_responses(response.into_inner(), inflight, Verb::Put, Some).await;
                }
                Err(status) => {
                    lore_debug!("put request failed: {status}");
                    fail_inflight(&inflight, Verb::Put, &ProtocolError::from(status));
                }
            }
        });

        handle
    }

    fn spawn_copy_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> StreamHandle<storage_v1::CopyRequest, ()> {
        let mut client = StorageServiceClient::new(self.connection.channel());
        let (tx, mut rx) =
            mpsc::channel::<StreamItem<storage_v1::CopyRequest, ()>>(STREAM_WRITE_BUFFER_SIZE);
        let handle = Arc::new(StreamState {
            sender: tx,
            opened: AtomicBool::new(false),
        });
        let state = handle.clone();
        let inflight: Arc<Inflight<()>> = Arc::new(DashMap::new());
        let routing = Arc::new(parking_lot::Mutex::new(CopyRouting::default()));
        let (resend, mut resent) = mpsc::unbounded_channel::<storage_v1::CopyRequest>();

        let request_inflight = inflight.clone();
        let request_routing = routing.clone();
        let requests = async_stream::stream! {
            loop {
                tokio::select! {
                    received = rx.recv() => {
                        let Some((request, sender)) = received else {
                            break;
                        };
                        let Some((source, target)) = CopyRouting::addresses(&request) else {
                            lore_debug!("Missing source_address or invalid target_context in copy request");
                            let _ = sender.send(Err(ProtocolError::internal("copy: invalid request")));
                            continue;
                        };
                        if !register(&request_inflight, target, sender) {
                            continue;
                        }
                        let admitted = request_routing.lock().admit(source, target, request);
                        if let Some(request) = admitted {
                            yield request;
                        }
                    }
                    Some(request) = resent.recv() => yield request,
                }
            }
        };
        let route = move |source| {
            let (target, next) = routing.lock().answered(source);
            if let Some(next) = next {
                let _ = resend.send(next);
            }
            target
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(requests);
            inject_metadata(&mut req, &ctx);

            match client.copy(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    pump_responses(response.into_inner(), inflight, Verb::Copy, route).await;
                }
                Err(status) => {
                    lore_debug!("copy request failed: {status}");
                    fail_inflight(&inflight, Verb::Copy, &ProtocolError::from(status));
                }
            }
        });

        handle
    }
}
