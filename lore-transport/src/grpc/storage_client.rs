// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
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

/// Keyed by the address the server echoes back. A `Vec` per key so concurrent requests for
/// the same address coalesce onto one round trip and all get woken by its response.
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

/// A per-item failure arrives in-band on an `Ok` message and costs only that request. An
/// `Err` is terminal by construction — tonic surfaces a stream status once and then reports
/// the stream exhausted — so it ends the loop and everything still outstanding fails.
///
/// Those outstanding requests are failed as `Disconnected` whatever the terminal code says,
/// because a stream status describes the stream rather than any one request on it. That is
/// also what lets `StreamCache::request` replay them: the ordinary ways a connection dies
/// arrive as `Internal` or `Cancelled` (`Status::from_h2_error`), neither of which a caller
/// would otherwise retry. The real status is kept on the error's trace.
async fn pump_responses<R>(
    mut stream: tonic::Streaming<R>,
    inflight: Arc<Inflight<R::Success>>,
    verb: Verb,
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
        let Some((_, senders)) = inflight.remove(&address) else {
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
    /// A death shows up three ways, all handled here: the send fails because the receiver is
    /// already gone (the payload comes back with the error, so nothing is lost), the reader
    /// answers with a disconnect, or it exits without answering at all. Anything else the
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

            let (tx, rx) = oneshot::channel();
            let answer = match handle.sender.send((payload.clone(), tx)).await {
                Ok(()) => rx.await.ok(),
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
            .map_err(|err| match err.code() {
                tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                _ => ProtocolError::internal_with_context(err, "query"),
            })?;

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
                tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                tonic::Code::NotFound => ProtocolError::from(NotFound),
                tonic::Code::Unimplemented => ProtocolError::internal("unsupported: verify"),
                _ => ProtocolError::internal_with_context(err, "verify"),
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
                tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                tonic::Code::NotFound => ProtocolError::from(NotFound),
                tonic::Code::Unimplemented => ProtocolError::internal("unsupported: mutable_load"),
                _ => ProtocolError::internal_with_context(err, "mutable_load"),
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
            .map_err(|err| match err.code() {
                tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                tonic::Code::Unimplemented => ProtocolError::internal("unsupported: mutable_store"),
                _ => ProtocolError::internal_with_context(err, "mutable_store"),
            })
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
            .map_err(|err| match err.code() {
                tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                tonic::Code::Unimplemented => ProtocolError::internal("unsupported: mutable_cas"),
                _ => ProtocolError::internal_with_context(err, "mutable_cas"),
            })?;

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
                    pump_responses(response.into_inner(), inflight, Verb::Get).await;
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
                    pump_responses(response.into_inner(), inflight, Verb::GetMetadata).await;
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
                    pump_responses(response.into_inner(), inflight, Verb::Put).await;
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

        let request_inflight = inflight.clone();
        let requests = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let Some(address) = request.source_address.as_ref().map(Address::from) else {
                    lore_debug!("Missing source_address in copy request");
                    let _ = sender.send(Err(ProtocolError::internal("copy: missing source_address")));
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

            match client.copy(req).await {
                Ok(response) => {
                    state.opened.store(true, Ordering::Relaxed);
                    pump_responses(response.into_inner(), inflight, Verb::Copy).await;
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_proto::lore::storage::v1::storage_service_server::StorageService as StorageServiceV1;
    use lore_proto::lore::storage::v1::storage_service_server::StorageServiceServer;
    use tokio_stream::Stream;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;
    use tonic::Streaming;

    use super::*;

    const TEST_PAYLOAD: &[u8] = b"rotation test payload";

    /// How the first `Get` stream a test server accepts is killed.
    ///
    /// Both leave requests unanswered with no per-request attribution, but they reach the
    /// client through different code: a clean end is `Streaming` reporting exhaustion, a status
    /// is the terminal `Err` arm. Real connection failures take the latter shape.
    #[derive(Clone, Copy)]
    enum KillMode {
        /// End the response body with nothing on it.
        CleanEnd,
        /// Terminate with the code an h2 connection failure produces.
        TerminalStatus,
        /// Never open the RPC at all, on any attempt.
        RefuseOpen,
        /// Refuse the first open, then serve normally — a channel that was down and came back.
        RefuseFirstOpen,
        /// Answer with a populated fragment and payload *and* a failure status.
        ErrorBesidePayload,
    }

    /// Kills the first `Get` stream it accepts, then serves every later stream normally.
    struct StreamKillingServer {
        accepted: Arc<AtomicUsize>,
        requests_read: Arc<AtomicUsize>,
        kill_mode: KillMode,
    }

    type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

    #[tonic::async_trait]
    impl StorageServiceV1 for StreamKillingServer {
        type GetStream = ResponseStream<storage_v1::GetResponse>;

        async fn get(
            &self,
            request: Request<Streaming<model_v1::Address>>,
        ) -> Result<Response<Self::GetStream>, Status> {
            let attempt = self.accepted.fetch_add(1, Ordering::SeqCst);
            let kill_mode = self.kill_mode;
            if let KillMode::RefuseOpen = kill_mode {
                return Err(Status::unavailable("refusing to open"));
            }
            if attempt == 0 && matches!(kill_mode, KillMode::RefuseFirstOpen) {
                return Err(Status::unavailable("refusing the first open"));
            }
            let mut requests = request.into_inner();
            let requests_read = self.requests_read.clone();

            let stream = async_stream::stream! {
                if attempt == 0 {
                    if let KillMode::TerminalStatus = kill_mode {
                        yield Err(Status::internal("connection reset"));
                    }
                    return;
                }
                while let Some(Ok(address)) = requests.next().await {
                    requests_read.fetch_add(1, Ordering::SeqCst);
                    let status = if matches!(kill_mode, KillMode::ErrorBesidePayload) {
                        lore_proto::lore::model::v1::ItemStatus {
                            code: i32::from(tonic::Code::NotFound) as u32,
                            message: "gone".to_string(),
                        }
                    } else {
                        lore_proto::lore::model::v1::ItemStatus::ok()
                    };
                    yield Ok(storage_v1::GetResponse {
                        address: Some(address),
                        fragment: Some(model_v1::Fragment {
                            flags: 0,
                            size_payload: TEST_PAYLOAD.len() as u32,
                            size_content: TEST_PAYLOAD.len() as u64,
                        }),
                        payload: Bytes::from_static(TEST_PAYLOAD),
                        status: Some(status),
                    });
                }
            };

            Ok(Response::new(Box::pin(stream) as Self::GetStream))
        }

        type GetMetadataStream = ResponseStream<storage_v1::GetResponse>;

        async fn get_metadata(
            &self,
            _request: Request<Streaming<model_v1::Address>>,
        ) -> Result<Response<Self::GetMetadataStream>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        type GetResolvedStream = ResponseStream<storage_v1::GetResolvedResponse>;

        async fn get_resolved(
            &self,
            request: Request<Streaming<storage_v1::GetResolvedRequest>>,
        ) -> Result<Response<Self::GetResolvedStream>, Status> {
            let mut requests = request.into_inner();
            let stream = async_stream::stream! {
                while let Some(Ok(req)) = requests.next().await {
                    yield Ok(storage_v1::GetResolvedResponse {
                        request_id: req.request_id,
                        ..Default::default()
                    });
                }
            };
            Ok(Response::new(Box::pin(stream) as Self::GetResolvedStream))
        }

        type PutResolvedStream = ResponseStream<storage_v1::PutResolvedResponse>;

        async fn put_resolved(
            &self,
            request: Request<Streaming<storage_v1::PutResolvedRequest>>,
        ) -> Result<Response<Self::PutResolvedStream>, Status> {
            let mut requests = request.into_inner();
            let stream = async_stream::stream! {
                while let Some(Ok(req)) = requests.next().await {
                    yield Ok(storage_v1::PutResolvedResponse {
                        request_id: req.request_id,
                        ..Default::default()
                    });
                }
            };
            Ok(Response::new(Box::pin(stream) as Self::PutResolvedStream))
        }

        type PutStream = ResponseStream<storage_v1::PutResponse>;

        async fn put(
            &self,
            request: Request<Streaming<storage_v1::PutRequest>>,
        ) -> Result<Response<Self::PutStream>, Status> {
            let mut requests = request.into_inner();
            let stream = async_stream::stream! {
                while let Some(Ok(req)) = requests.next().await {
                    yield Ok(storage_v1::PutResponse {
                        address: req.address,
                        status: None,
                    });
                }
            };
            Ok(Response::new(Box::pin(stream) as Self::PutStream))
        }

        type CopyStream = ResponseStream<storage_v1::CopyResponse>;

        async fn copy(
            &self,
            _request: Request<Streaming<storage_v1::CopyRequest>>,
        ) -> Result<Response<Self::CopyStream>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn query(
            &self,
            _request: Request<storage_v1::QueryRequest>,
        ) -> Result<Response<storage_v1::QueryResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn verify(
            &self,
            _request: Request<storage_v1::VerifyRequest>,
        ) -> Result<Response<storage_v1::VerifyResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn mutable_load(
            &self,
            _request: Request<storage_v1::MutableLoadRequest>,
        ) -> Result<Response<storage_v1::MutableLoadResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn mutable_store(
            &self,
            _request: Request<storage_v1::MutableStoreRequest>,
        ) -> Result<Response<storage_v1::MutableStoreResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }

        async fn mutable_compare_and_swap(
            &self,
            _request: Request<storage_v1::MutableCompareAndSwapRequest>,
        ) -> Result<Response<storage_v1::MutableCompareAndSwapResponse>, Status> {
            Err(Status::unimplemented("not used by this test"))
        }
    }

    fn test_context() -> GrpcSessionContext {
        GrpcSessionContext {
            partition: Partition::from(Context::from([0x11u8; 16])),
            correlation_id: "rotation-test".to_string(),
            auth_token: String::new(),
        }
    }

    /// Stand up a `StorageService` talking to a `StreamKillingServer`, and hand back the
    /// counter of streams the server has accepted.
    async fn start_test_service(
        kill_mode: KillMode,
    ) -> (StorageService, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (connection, accepted, requests_read) = start_test_connection(kill_mode).await;
        (StorageService::new(connection), accepted, requests_read)
    }

    /// The same server and a connection over it, for tests that drive the connection directly
    /// rather than through a storage client.
    async fn start_test_connection(
        kill_mode: KillMode,
    ) -> (
        Arc<super::super::GRPCConnection>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let accepted = Arc::new(AtomicUsize::new(0));
        let requests_read = Arc::new(AtomicUsize::new(0));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let server = StreamKillingServer {
            accepted: accepted.clone(),
            requests_read: requests_read.clone(),
            kill_mode,
        };

        #[allow(clippy::disallowed_methods)] // Test-local server task.
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(StorageServiceServer::new(server))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .expect("connect to test server");
        let channel = tower::ServiceBuilder::new()
            .layer(super::super::RequestLoggerLayer {})
            .service(channel);

        let connection = Arc::new(super::super::GRPCConnection::for_test(
            format!("http://{addr}").parse().expect("test url"),
            channel,
        ));
        (connection, accepted, requests_read)
    }

    /// Drive one `get` against a server that kills its first stream, and report how many
    /// streams the server ended up accepting.
    async fn get_against_stream_killing_server(
        kill_mode: KillMode,
    ) -> (Result<(Fragment, Bytes), ProtocolError>, usize) {
        let (service, accepted, _) = start_test_service(kill_mode).await;
        let ctx = test_context();
        let address = Address::zero_context_hash(Hash::from([0x22u8; 32]));

        let result = service.get(0, &ctx, &address).await;

        (result, accepted.load(Ordering::SeqCst))
    }

    /// A stream that ends without answering is re-established and the request replayed.
    ///
    /// The caller must see one successful fetch, not a transport error. Before, the dead channel
    /// stayed cached and every later request on the session failed against it.
    #[tokio::test]
    async fn get_rotates_when_the_first_stream_ends_without_answering() {
        let (result, streams) = get_against_stream_killing_server(KillMode::CleanEnd).await;
        let (fragment, payload) = result.expect("get must recover by re-establishing the stream");

        assert_eq!(payload.as_ref(), TEST_PAYLOAD);
        assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
        assert_eq!(
            streams, 2,
            "the server must have seen a second Get stream — the first was killed",
        );
    }

    /// The same recovery when the stream dies with a terminal status rather than ending.
    ///
    /// This is the shape a real connection failure takes, and the codes it carries — `Internal`,
    /// `Cancelled` — are not `Disconnected` on their own. Unless a stream status is treated as a
    /// stream death regardless of its code, the request is handed back to the caller as a plain
    /// error and the rotation never happens.
    #[tokio::test]
    async fn get_rotates_when_the_first_stream_dies_with_a_terminal_status() {
        let (result, streams) = get_against_stream_killing_server(KillMode::TerminalStatus).await;
        let (fragment, payload) = result.expect("get must recover by re-establishing the stream");

        assert_eq!(payload.as_ref(), TEST_PAYLOAD);
        assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
        assert_eq!(
            streams, 2,
            "an Internal stream status must rotate onto a new stream, not fail the request",
        );
    }

    /// Many requests losing a stream together must cost exactly one replacement.
    ///
    /// Single-flight rotation rests on `Arc::ptr_eq` against the cached handle under the shard
    /// lock. If that check regressed, every waiter would open its own stream — still correct, so
    /// nothing else would fail, but the stream count would scale with concurrency.
    #[tokio::test]
    async fn concurrent_requests_share_one_replacement_stream() {
        let (service, accepted, _) = start_test_service(KillMode::CleanEnd).await;
        let ctx = test_context();

        let addresses: Vec<Address> = (0..16u8)
            .map(|i| Address::zero_context_hash(Hash::from([i; 32])))
            .collect();
        let results = futures::future::join_all(
            addresses
                .iter()
                .map(|address| service.get(0, &ctx, address)),
        )
        .await;

        for result in &results {
            assert!(result.is_ok(), "every request must recover: {result:?}");
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "the killed stream must be replaced once for all waiters, not once each",
        );
    }

    /// Concurrent requests for one address ride a single round trip.
    ///
    /// `register` returns false for later waiters on a key so they attach to the request already
    /// on the wire. A regression here costs duplicate wire traffic on duplicate reads without
    /// failing anything, so only the server's request count catches it. The warm-up get spends
    /// the killed first stream, leaving a live one for the coalescing itself.
    #[tokio::test]
    async fn concurrent_requests_for_one_address_coalesce() {
        let (service, _, requests_read) = start_test_service(KillMode::CleanEnd).await;
        let ctx = test_context();
        let address = Address::zero_context_hash(Hash::from([0x44u8; 32]));

        service
            .get(
                0,
                &ctx,
                &Address::zero_context_hash(Hash::from([0x45u8; 32])),
            )
            .await
            .expect("warm-up get");
        requests_read.store(0, Ordering::SeqCst);

        let (first, second) = tokio::join!(
            service.get(0, &ctx, &address),
            service.get(0, &ctx, &address)
        );

        assert_eq!(first.expect("first get").1.as_ref(), TEST_PAYLOAD);
        assert_eq!(second.expect("second get").1.as_ref(), TEST_PAYLOAD);
        assert_eq!(
            requests_read.load(Ordering::SeqCst),
            1,
            "duplicate addresses must coalesce onto one round trip",
        );
    }

    /// Concurrent callers losing one channel must rebuild it once between them.
    ///
    /// This is the real machinery, not a stand-in: `GRPCConnection::reconnect` serialises callers
    /// on `reconnector` and then compares the epoch each carries against the current one, so the
    /// first through does the connect and the rest adopt its channel. The epoch delta is the
    /// observable — it advances once per actual rebuild, so N concurrent callers that each
    /// rebuilt would leave it N higher.
    #[tokio::test]
    async fn concurrent_reconnects_rebuild_the_channel_once() {
        let (connection, _, _) = start_test_connection(KillMode::CleanEnd).await;
        let epoch_before = connection.reconnect.load(Ordering::Relaxed);

        let results = futures::future::join_all((0..8).map(|_| {
            let connection = connection.clone();
            async move { connection.reconnect(epoch_before).await.map(|_| ()) }
        }))
        .await;

        for result in &results {
            assert!(
                result.is_ok(),
                "every caller must get a channel: {result:?}"
            );
        }
        assert_eq!(
            connection.reconnect.load(Ordering::Relaxed),
            epoch_before + 1,
            "eight concurrent callers must cost exactly one rebuild",
        );
    }

    /// A failure status decides the item even when the payload fields are populated.
    ///
    /// The fields kept their original numbers so an older peer still decodes them, which means a
    /// response can carry both a fragment and a failure. The status wins: consulting `fragment`
    /// first would turn a reported miss into a served fragment.
    #[tokio::test]
    async fn a_failure_status_wins_over_a_populated_payload() {
        let (service, _, _) = start_test_service(KillMode::ErrorBesidePayload).await;
        let ctx = test_context();
        let address = Address::zero_context_hash(Hash::from([0x77u8; 32]));

        let err = service
            .get(0, &ctx, &address)
            .await
            .expect_err("a failure status must not be overridden by the payload fields");

        assert!(
            err.is_not_found(),
            "the server's code must survive, got {err:?}",
        );
    }

    /// A refused open must not poison the cache for later requests.
    ///
    /// Giving up leaves a handle whose stream never established. A later request that adopts it
    /// from the cache would fail on its send, see `opened` still false, and give up again without
    /// ever trying a fresh stream — so a channel that came back would never be used.
    #[tokio::test]
    async fn a_refused_open_does_not_poison_the_cached_stream() {
        let (service, accepted, _) = start_test_service(KillMode::RefuseFirstOpen).await;
        let ctx = test_context();
        let address = Address::zero_context_hash(Hash::from([0x55u8; 32]));

        let refused = service.get(0, &ctx, &address).await;
        assert!(
            refused.is_err_and(|err| err.is_disconnected()),
            "the refused open must surface as a disconnect",
        );

        let (fragment, payload) = service
            .get(0, &ctx, &address)
            .await
            .expect("a later request must open a fresh stream, not inherit the dead one");

        assert_eq!(payload.as_ref(), TEST_PAYLOAD);
        assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "exactly one refused open and one that served",
        );
    }

    /// A storage operation recovers across a connection-level reconnect.
    ///
    /// This is the seam the two layers meet at, and the one place neither covers alone: the stream
    /// cache reports a refused open as `Disconnected`, `GRPCStorage::with_reconnect` rebuilds the
    /// channel, and the retried operation must open a stream on the new one and succeed. The
    /// caller sees a single successful fetch. Asserting the epoch advanced proves a real reconnect
    /// happened rather than the first attempt merely being retried.
    #[tokio::test]
    async fn a_storage_operation_recovers_across_a_reconnect() {
        use crate::traits::Storage;

        let (connection, accepted, _) = start_test_connection(KillMode::RefuseFirstOpen).await;
        let epoch_before = connection.reconnect.load(Ordering::SeqCst);

        let storage = super::super::GRPCStorage {
            connection: connection.clone(),
            client: StorageService::new(connection.clone()),
            auth_url: String::new(),
            identity: String::new(),
            credentials: Arc::new(super::super::SuppliedCredentials::default()),
            session_counter: std::sync::atomic::AtomicU32::new(1),
            sessions: DashMap::new(),
        };
        storage.sessions.insert(0, Arc::new(test_context()));

        let address = Address::zero_context_hash(Hash::from([0x66u8; 32]));
        let (fragment, payload) = storage
            .get(0, &address)
            .await
            .expect("the operation must recover once the channel is rebuilt");

        assert_eq!(payload.as_ref(), TEST_PAYLOAD);
        assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
        assert_eq!(
            connection.reconnect.load(Ordering::SeqCst),
            epoch_before + 1,
            "recovery must have gone through a real reconnect",
        );
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "one refused open, then one on the rebuilt channel",
        );
    }

    /// A put response without a status is read as success, for a peer that predates the field.
    ///
    /// The status field is additive: a server that does not set it reports a per-item failure by
    /// ending the stream, exactly as it did before the field existed. Treating silence as a
    /// failure would break every put against such a server.
    #[tokio::test]
    async fn put_treats_a_missing_status_as_success() {
        let (service, _accepted, _) = start_test_service(KillMode::CleanEnd).await;
        let ctx = test_context();
        let address = Address::zero_context_hash(Hash::from([0x33u8; 32]));
        let fragment = Fragment {
            flags: 0,
            size_payload: TEST_PAYLOAD.len() as u32,
            size_content: TEST_PAYLOAD.len() as u64,
        };

        service
            .put(
                0,
                &ctx,
                address,
                fragment,
                Some(Bytes::from_static(TEST_PAYLOAD)),
            )
            .await
            .expect("a status-less put response must be taken as success");
    }

    /// A stream that will not open hands straight off rather than reissuing.
    ///
    /// Reissuing only covers a stream dying on a healthy channel. A refused open means the
    /// channel is suspect, which is `GRPCConnection::reconnect`'s job, so this must report
    /// `Disconnected` after a single attempt instead of spending the reissue budget.
    #[tokio::test]
    async fn get_gives_up_once_the_stream_will_not_open() {
        let (result, opens) = get_against_stream_killing_server(KillMode::RefuseOpen).await;

        let err = result.expect_err("a remote refusing every open must fail the request");
        assert!(
            err.is_disconnected(),
            "giving up must report a disconnect, got {err:?}",
        );
        assert_eq!(
            opens, 1,
            "a refused open must hand off immediately, not reissue",
        );
    }
}
