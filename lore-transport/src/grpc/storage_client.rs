// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
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
use lore_base::types::RepositoryId;
use lore_base::types::VerifyResult;
use lore_error_set::prelude::*;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_proto::lore::storage::v1::storage_service_client::StorageServiceClient;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tonic::metadata::MetadataValue;

use super::CORRELATION_ID_HEADER;
use super::Channel;
use super::PARTITION_ID_KEY;
use super::REPOSITORY_ID_KEY;
use crate::error::ProtocolError;

type GetResponseSender = oneshot::Sender<Result<Arc<storage_v1::GetResponse>, ProtocolError>>;
type PutResponseSender = oneshot::Sender<Result<Arc<storage_v1::PutResponse>, ProtocolError>>;
type CopyResponseSender = oneshot::Sender<Result<(), ProtocolError>>;

const STREAM_WRITE_BUFFER_SIZE: usize = 32 * 1024;
const INFLIGHT_COMMAND_LIMIT: usize = 10000;

/// Session context for gRPC metadata injection. Cached at `session_start` time.
#[derive(Clone)]
pub struct GrpcSessionContext {
    pub repository: RepositoryId,
    pub correlation_id: String,
    pub auth_token: String,
}

/// Per-session streaming state keyed by `(repository, correlation_id)`.
struct SessionStreams {
    get_stream: tokio::sync::OnceCell<mpsc::Sender<(Address, GetResponseSender)>>,
    get_metadata_stream: tokio::sync::OnceCell<mpsc::Sender<(Address, GetResponseSender)>>,
    put_stream: tokio::sync::OnceCell<mpsc::Sender<(storage_v1::PutRequest, PutResponseSender)>>,
    copy_stream: tokio::sync::OnceCell<mpsc::Sender<(storage_v1::CopyRequest, CopyResponseSender)>>,
}

pub struct StorageService {
    client: StorageServiceClient<Channel>,
    /// Per-session streams keyed by session ID.
    streams: DashMap<u32, Arc<SessionStreams>>,
    get_put_limiter: Semaphore,
}

fn inject_metadata<T>(request: &mut tonic::Request<T>, ctx: &GrpcSessionContext) {
    let md = request.metadata_mut();
    md.insert_bin(
        PARTITION_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(ctx.repository.data()),
    );
    md.insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(ctx.repository.data()),
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
    pub fn new(channel: Channel) -> Self {
        let client = StorageServiceClient::new(channel);

        Self {
            client,
            streams: DashMap::new(),
            get_put_limiter: Semaphore::new(INFLIGHT_COMMAND_LIMIT),
        }
    }

    /// Remove streams for a session. Dropping the senders terminates the stream tasks.
    pub fn remove_session_streams(&self, session_id: u32) {
        self.streams.remove(&session_id);
    }

    fn session_streams(&self, session_id: u32) -> Arc<SessionStreams> {
        #[allow(clippy::disallowed_methods)] // Write lock is brief; no await while held.
        self.streams
            .entry(session_id)
            .or_insert_with(|| {
                Arc::new(SessionStreams {
                    get_stream: tokio::sync::OnceCell::new(),
                    get_metadata_stream: tokio::sync::OnceCell::new(),
                    put_stream: tokio::sync::OnceCell::new(),
                    copy_stream: tokio::sync::OnceCell::new(),
                })
            })
            .clone()
    }

    pub async fn get(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        lore_debug!("gRPC get fragment: {}", address);

        let streams = self.session_streams(session_id);
        let get_stream = streams
            .get_stream
            .get_or_try_init(|| async {
                self.spawn_get_stream(ctx).internal("spawning get stream")
            })
            .await?
            .clone();

        let permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;
        let (tx, rx) = oneshot::channel();
        let res = match get_stream.send((*address, tx)).await {
            Ok(_) => rx.await.unwrap_or_else(|err| {
                lore_error!("Error receiving get result from channel: {err}");
                Err(ProtocolError::internal_with_context(err, "get"))
            }),
            Err(err) => {
                lore_error!("Error sending fragment to get channel: {err}");
                Err(ProtocolError::internal_with_context(err, "get"))
            }
        }?;

        drop(permit);

        let Some(fragment) = res.fragment else {
            lore_error!("Invalid get response, missing fragment");
            return Err(ProtocolError::internal("get: Missing fragment"));
        };

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
        if res.payload.len() != fragment.size_payload as usize {
            lore_error!(
                "Fragment payload is invalid in get response : {} bytes, expected {}",
                res.payload.len(),
                fragment.size_payload
            );
            return Err(ProtocolError::internal("get: Invalid payload"));
        }

        Ok((fragment, res.payload.clone()))
    }

    /// Fetch only the fragment metadata for an address. Same wire request as `get` (just an
    /// `Address`), but the server's response carries `Fragment` only — no payload bytes — so
    /// callers that don't need the payload skip the transfer cost. Used by the storage API's
    /// query op for remote-hit metadata lookups.
    pub async fn get_metadata(
        &self,
        session_id: u32,
        ctx: &GrpcSessionContext,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        lore_debug!("gRPC get_metadata fragment: {}", address);

        let streams = self.session_streams(session_id);
        let stream = streams
            .get_metadata_stream
            .get_or_try_init(|| async {
                self.spawn_get_metadata_stream(ctx)
                    .internal("spawning get_metadata stream")
            })
            .await?
            .clone();

        let permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;
        let (tx, rx) = oneshot::channel();
        let res = match stream.send((*address, tx)).await {
            Ok(_) => rx.await.unwrap_or_else(|err| {
                lore_error!("Error receiving get_metadata result from channel: {err}");
                Err(ProtocolError::internal_with_context(err, "get_metadata"))
            }),
            Err(err) => {
                lore_error!("Error sending fragment to get_metadata channel: {err}");
                Err(ProtocolError::internal_with_context(err, "get_metadata"))
            }
        }?;

        drop(permit);

        let Some(fragment) = res.fragment else {
            lore_error!("Invalid get_metadata response, missing fragment");
            return Err(ProtocolError::internal("get_metadata: Missing fragment"));
        };

        Ok(Fragment {
            flags: fragment.flags,
            size_payload: fragment.size_payload,
            size_content: fragment.size_content,
        })
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

        let streams = self.session_streams(session_id);
        let put_stream = streams
            .put_stream
            .get_or_try_init(|| async {
                self.spawn_put_stream(ctx).internal("spawning put stream")
            })
            .await?
            .clone();

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;
        let (tx, rx) = oneshot::channel();
        let v1_address: model_v1::Address = address.into();
        let v1_fragment: model_v1::Fragment = fragment.into();
        match put_stream
            .send((
                storage_v1::PutRequest {
                    address: Some(v1_address),
                    fragment: Some(v1_fragment),
                    payload,
                },
                tx,
            ))
            .await
        {
            Ok(_) => rx.await.unwrap_or_else(|err| {
                lore_error!("Error receiving put result from channel: {err}");
                Err(ProtocolError::internal_with_context(err, "put"))
            }),
            Err(err) => {
                lore_error!("Error sending put request to channel: {err}");
                Err(ProtocolError::internal_with_context(err, "put"))
            }
        }?;

        Ok(())
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
        let mut client = self.client.clone();
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
        let mut client = self.client.clone();
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
        source_repository: RepositoryId,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        lore_debug!(
            "gRPC copy fragment: {} from repository {} (target context {})",
            source_address,
            source_repository,
            target_context
        );

        let streams = self.session_streams(session_id);
        let copy_stream = streams
            .copy_stream
            .get_or_try_init(|| async {
                self.spawn_copy_stream(ctx).internal("spawning copy stream")
            })
            .await?
            .clone();

        let _permit = self
            .get_put_limiter
            .acquire()
            .await
            .internal("permit acquire")?;
        let (tx, rx) = oneshot::channel();
        match copy_stream
            .send((
                storage_v1::CopyRequest {
                    source_repository_id: Bytes::copy_from_slice(source_repository.data()),
                    source_address: Some(source_address.into()),
                    target_context: Bytes::copy_from_slice(zerocopy::IntoBytes::as_bytes(
                        &target_context,
                    )),
                },
                tx,
            ))
            .await
        {
            Ok(_) => rx.await.unwrap_or_else(|err| {
                lore_error!("Error receiving copy result from channel: {err}");
                Err(ProtocolError::internal_with_context(err, "copy"))
            }),
            Err(err) => {
                lore_error!("Error sending copy request to channel: {err}");
                Err(ProtocolError::internal_with_context(err, "copy"))
            }
        }
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
        let mut client = self.client.clone();
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
        let mut client = self.client.clone();
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
        let mut client = self.client.clone();
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

    fn spawn_copy_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> Result<mpsc::Sender<(storage_v1::CopyRequest, CopyResponseSender)>, ProtocolError> {
        let mut client = self.client.clone();
        let (tx, mut rx) = mpsc::channel::<(storage_v1::CopyRequest, CopyResponseSender)>(
            STREAM_WRITE_BUFFER_SIZE,
        );

        let inflight = Arc::new(DashMap::<Address, Vec<CopyResponseSender>>::new());

        let request_inflight = inflight.clone();
        let request_stream = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let Some(address) = request.source_address.clone() else {
                    lore_error!("Missing source_address in copy request");
                    break;
                };

                let mut send = false;
                #[allow(clippy::disallowed_methods)]
                {
                    let address = Address::from(address);
                    request_inflight.entry(address).or_insert_with(|| { send = true; vec![] }).push(sender);
                }
                if send {
                    yield request;
                }
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!({
            async move {
                let mut req = tonic::Request::new(request_stream);
                inject_metadata(&mut req, &ctx);

                let mut response_stream = client
                    .copy(req)
                    .await
                    .map_err(|err| match err.code() {
                        tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                        tonic::Code::NotFound => ProtocolError::from(NotFound),
                        tonic::Code::Unimplemented => ProtocolError::internal("unsupported: copy"),
                        _ => ProtocolError::internal_with_context(err, "copy"),
                    })?
                    .into_inner();

                while let Some(response) = response_stream.next().await {
                    match response {
                        Ok(response) => {
                            let Some(address) = response.source_address.clone() else {
                                return Err(ProtocolError::internal(
                                    "copy: Copy response missing source_address",
                                ));
                            };
                            let address = Address::from(address);
                            let senders = inflight.remove(&address);
                            if let Some((_, senders)) = senders {
                                for sender in senders {
                                    let _ = sender.send(Ok(())).map_err(|err| {
                                        lore_error!("Copy request failed sending response to requestor: {err:?}");
                                    });
                                }
                            } else {
                                lore_error!(
                                    "Received unexpected copy result for fragment address: {address}",
                                );
                            }
                        }
                        Err(e) => {
                            lore_error!("Error during fragment copy: {e}");
                            let address = Address::from(e.details());
                            if !address.is_zero() {
                                let senders = inflight.remove(&address);
                                if let Some((_, senders)) = senders {
                                    // Preserve the original status code (NotFound,
                                    // NotAuthorized, Unavailable, …) by going through
                                    // `ProtocolError::From<tonic::Status>` rather than
                                    // wrapping every error as Internal. Callers downstream
                                    // (e.g. `lore::storage::copy`'s tier-2 fallback) need to
                                    // pattern-match on the variant to decide whether a
                                    // recovery path makes sense.
                                    let err = ProtocolError::from(e);
                                    for sender in senders {
                                        let _ = sender.send(Err(err.clone())).map_err(|err| {
                                            lore_error!("Copy request failed sending error response to requestor: {err:?}");
                                        });
                                    }
                                } else {
                                    lore_error!(
                                        "Missing hash received from Status error: {:?}",
                                        address.hash
                                    );
                                }
                            } else {
                                lore_error!(
                                    "Copy request failed and no address details found: {e}"
                                );
                            }
                        }
                    }
                }

                Ok(())
            }
        });

        Ok(tx)
    }

    fn spawn_get_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> Result<mpsc::Sender<(Address, GetResponseSender)>, ProtocolError> {
        let mut client = self.client.clone();
        let (tx, mut rx) = mpsc::channel::<(Address, GetResponseSender)>(STREAM_WRITE_BUFFER_SIZE);

        let inflight = Arc::new(DashMap::<Address, Vec<GetResponseSender>>::new());

        let request_inflight = inflight.clone();
        let request = async_stream::stream! {
            while let Some((address, sender)) = rx.recv().await {
                let mut send = false;
                #[allow(clippy::disallowed_methods)]
                {
                    request_inflight.entry(address).or_insert_with(|| { send = true; vec![] }).push(sender);
                }
                if send {
                    let v1_address: model_v1::Address = address.into();
                    yield v1_address;
                }
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(request);
            inject_metadata(&mut req, &ctx);

            let mut response_stream = client
                .get(req)
                .await
                .map_err(|err| {
                    lore_error!("Get request failed: {err}");
                    match err.code() {
                        tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                        _ => ProtocolError::internal(format!("get: {err}")),
                    }
                })?
                .into_inner();

            while let Some(response) = response_stream.next().await {
                match response {
                    Ok(response) => {
                        let Some(address) = response.address.as_ref() else {
                            return Err(ProtocolError::internal(
                                "get: Get response missing address",
                            ));
                        };
                        let senders = inflight.remove(&Address::from(address));
                        if let Some((_, senders)) = senders {
                            let response = Arc::new(response);
                            for sender in senders {
                                let _ = sender.send(Ok(response.clone())).map_err(|err| {
                                    lore_error!(
                                        "Get request failed sending response to requestor: {err:?}"
                                    );
                                });
                            }
                        } else {
                            lore_error!(
                                "Received unexpected result for fragment address during get stream: {:?}",
                                address.hash
                            );
                        }
                    }
                    Err(e) => {
                        let address = Address::from(e.details());
                        if !address.is_zero() {
                            let senders = inflight.remove(&address);
                            if let Some((_, senders)) = senders {
                                for sender in senders {
                                    let _ = sender.send(Err(ProtocolError::internal(format!("get: {e}")))).map_err(|err| {
                                            lore_error!("Get request failed sending error response to requestor: {err:?}");
                                        });
                                }
                            } else {
                                lore_error!("Missing hash received with error: {e:?}");
                            }
                        } else {
                            lore_error!("Get request failed and no address details found: {e:?}");
                        }
                    }
                }
            }

            Ok(())
        });

        Ok(tx)
    }

    fn spawn_get_metadata_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> Result<mpsc::Sender<(Address, GetResponseSender)>, ProtocolError> {
        let mut client = self.client.clone();
        let (tx, mut rx) = mpsc::channel::<(Address, GetResponseSender)>(STREAM_WRITE_BUFFER_SIZE);

        let inflight = Arc::new(DashMap::<Address, Vec<GetResponseSender>>::new());

        let request_inflight = inflight.clone();
        let request = async_stream::stream! {
            while let Some((address, sender)) = rx.recv().await {
                let mut send = false;
                #[allow(clippy::disallowed_methods)]
                {
                    request_inflight.entry(address).or_insert_with(|| { send = true; vec![] }).push(sender);
                }
                if send {
                    let v1_address: model_v1::Address = address.into();
                    yield v1_address;
                }
            }
        };

        let ctx = ctx.clone();
        lore_spawn_net!(async move {
            let mut req = tonic::Request::new(request);
            inject_metadata(&mut req, &ctx);

            let mut response_stream = client
                .get_metadata(req)
                .await
                .map_err(|err| {
                    lore_error!("GetMetadata request failed: {err}");
                    match err.code() {
                        tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                        _ => ProtocolError::internal(format!("get_metadata: {err}")),
                    }
                })?
                .into_inner();

            while let Some(response) = response_stream.next().await {
                match response {
                    Ok(response) => {
                        let Some(address) = response.address.as_ref() else {
                            return Err(ProtocolError::internal(
                                "get_metadata: response missing address",
                            ));
                        };
                        let senders = inflight.remove(&Address::from(address));
                        if let Some((_, senders)) = senders {
                            let response = Arc::new(response);
                            for sender in senders {
                                let _ = sender.send(Ok(response.clone())).map_err(|err| {
                                    lore_error!(
                                        "GetMetadata sending response to requestor failed: {err:?}"
                                    );
                                });
                            }
                        } else {
                            lore_error!(
                                "GetMetadata received unexpected result for fragment address: {:?}",
                                address.hash
                            );
                        }
                    }
                    Err(e) => {
                        let address = Address::from(e.details());
                        if !address.is_zero() {
                            let senders = inflight.remove(&address);
                            if let Some((_, senders)) = senders {
                                // Preserve original status code so callers can match on
                                // `NotFound` etc., mirroring the copy-stream fix.
                                let err = ProtocolError::from(e);
                                for sender in senders {
                                    let _ = sender.send(Err(err.clone())).map_err(|send_err| {
                                        lore_error!(
                                            "GetMetadata sending error to requestor failed: {send_err:?}"
                                        );
                                    });
                                }
                            } else {
                                lore_error!("GetMetadata missing hash with error: {e:?}");
                            }
                        } else {
                            lore_error!(
                                "GetMetadata request failed and no address details found: {e:?}"
                            );
                        }
                    }
                }
            }

            Ok(())
        });

        Ok(tx)
    }

    fn spawn_put_stream(
        &self,
        ctx: &GrpcSessionContext,
    ) -> Result<Sender<(storage_v1::PutRequest, PutResponseSender)>, ProtocolError> {
        let mut client = self.client.clone();
        let (tx, mut rx) =
            mpsc::channel::<(storage_v1::PutRequest, PutResponseSender)>(STREAM_WRITE_BUFFER_SIZE);

        let inflight = Arc::new(DashMap::<Address, Vec<PutResponseSender>>::new());

        let request_inflight = inflight.clone();
        let request_stream = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let Some(address) = request.address.clone() else {
                    lore_error!("Missing address");
                    break;
                };

                let mut send = false;
                #[allow(clippy::disallowed_methods)]
                {
                    let address = Address::from(address);
                    request_inflight.entry(address).or_insert_with(|| { send = true; vec![] }).push(sender);
                }
                if send {
                    yield request;
                }
            }
        };

        let request = request_stream;

        let ctx = ctx.clone();
        lore_spawn_net!({
            async move {
                let mut req = tonic::Request::new(request);
                inject_metadata(&mut req, &ctx);

                let mut response_stream = client
                    .put(req)
                    .await
                    .map_err(|err| match err.code() {
                        tonic::Code::Unavailable => ProtocolError::from(SlowDown),
                        _ => ProtocolError::internal(format!("put: {err}")),
                    })?
                    .into_inner();

                while let Some(response) = response_stream.next().await {
                    match response {
                        Ok(response) => {
                            let Some(address) = response.address.clone() else {
                                return Err(ProtocolError::internal(
                                    "put: Put response missing address",
                                ));
                            };
                            let address = Address::from(address);
                            let senders = inflight.remove(&address);
                            if let Some((_, senders)) = senders {
                                let response = Arc::new(response);
                                for sender in senders {
                                    let _ = sender.send(Ok(response.clone())).map_err(|err| {
                                        lore_error!("Put request failed sending response to requestor: {err:?}");
                                    });
                                }
                            } else {
                                lore_error!(
                                    "Received unexpected put result for fragment address: {address}",
                                );
                            }
                        }
                        Err(e) => {
                            lore_error!("Error during fragment put: {e}");
                            let address = Address::from(e.details());
                            if !address.is_zero() {
                                let senders = inflight.remove(&address);
                                if let Some((_, senders)) = senders {
                                    for sender in senders {
                                        let _ = sender
                                            .send(Err(ProtocolError::internal(format!("put: {e}")))).map_err(|err| {
                                            lore_error!("Put request failed sending error response to requestor: {err:?}");
                                        });
                                    }
                                } else {
                                    lore_error!(
                                        "Missing hash received from Status error: {:?}",
                                        address.hash
                                    );
                                }
                            } else {
                                lore_error!("Put request failed and no address details found: {e}");
                            }
                        }
                    }
                }

                Ok(())
            }
        });

        Ok(tx)
    }
}
