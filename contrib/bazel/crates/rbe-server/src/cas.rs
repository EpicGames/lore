// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `ContentAddressableStorage`, `ByteStream` and `Capabilities`.
//!
//! The CAS is the "build data, intermediates and outputs" half of the cache: every action
//! input bazel uploads and every output a worker produces lands here, keyed by its SHA-256
//! digest, in the Lore `Ns::Cas` namespace. `ByteStream` is the same store reached one blob at
//! a time, for blobs above the batch limit.
//!
//! Both offer zstd (REAPI `Compressor.ZSTD`): a client may send and fetch blobs compressed, through
//! `compressed-blobs/zstd/...` resource names and the batch calls' `compressor` fields. Digests
//! always name the uncompressed content, which is what Lore stores; compression exists only on
//! the wire between the client and this server. Workers reach Lore directly and never see it.

use std::borrow::Cow;
use std::io::Write as _;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use futures::Stream;
use rbe_lore::Delivery;
use rbe_lore::EMPTY_ZSTD_FRAME;
use rbe_lore::LoreBlobStore;
use rbe_lore::Ns;
use rbe_lore::digest::EMPTY_SHA256;
use rbe_lore::digest::is_empty_digest;
use rbe_lore::digest::key_of;
use rbe_lore::digest::{self};
use rbe_proto::bytestream::QueryWriteStatusRequest;
use rbe_proto::bytestream::QueryWriteStatusResponse;
use rbe_proto::bytestream::ReadRequest;
use rbe_proto::bytestream::ReadResponse;
use rbe_proto::bytestream::WriteRequest;
use rbe_proto::bytestream::WriteResponse;
use rbe_proto::bytestream::byte_stream_server::ByteStream;
use rbe_proto::reapi::ActionCacheUpdateCapabilities;
use rbe_proto::reapi::BatchReadBlobsRequest;
use rbe_proto::reapi::BatchReadBlobsResponse;
use rbe_proto::reapi::BatchUpdateBlobsRequest;
use rbe_proto::reapi::BatchUpdateBlobsResponse;
use rbe_proto::reapi::CacheCapabilities;
use rbe_proto::reapi::Digest;
use rbe_proto::reapi::Directory;
use rbe_proto::reapi::ExecutionCapabilities;
use rbe_proto::reapi::GetCapabilitiesRequest;
use rbe_proto::reapi::GetTreeRequest;
use rbe_proto::reapi::GetTreeResponse;
use rbe_proto::reapi::ServerCapabilities;
use rbe_proto::reapi::batch_read_blobs_response;
use rbe_proto::reapi::batch_update_blobs_response;
use rbe_proto::reapi::capabilities_server::Capabilities;
use rbe_proto::reapi::compressor;
use rbe_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use rbe_proto::reapi::digest_function;
use rbe_proto::reapi::symlink_absolute_path_strategy;
use rbe_proto::semver::SemVer;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;

/// What `CacheCapabilities.max_batch_total_size_bytes` advertises, and the ceiling this server
/// enforces on a batch. Matches bazel's own default, so bazel splits batches for us.
pub const MAX_BATCH_TOTAL_BYTES: i64 = 4 * 1024 * 1024;

/// Chunk size for `ByteStream.Read` responses. Large enough that a multi-MiB blob is a handful
/// of frames, small enough to stay well under any gRPC frame limit.
const READ_CHUNK_BYTES: usize = 1024 * 1024;

fn ok_status() -> rbe_proto::rpc::Status {
    rbe_proto::rpc::Status {
        code: tonic::Code::Ok as i32,
        message: String::new(),
        details: Vec::new(),
    }
}

fn err_status(code: tonic::Code, message: impl Into<String>) -> rbe_proto::rpc::Status {
    rbe_proto::rpc::Status {
        code: code as i32,
        message: message.into(),
        details: Vec::new(),
    }
}

fn internal(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}

/// zstd level for what this server compresses on the way out: zstd's own default, which
/// compresses much faster than WiFi or 5 GbE can carry the result.
const ZSTD_LEVEL: i32 = 3;

/// Above this, compressing, decompressing and hashing a blob move to the blocking pool: a level-3
/// pass or a SHA-256 over a 100 MB test binary takes a few hundred milliseconds, which on a
/// runtime thread would stall every other request scheduled there. Below it the hop costs more
/// than the work.
#[lore_macro::test_pub]
const OFF_RUNTIME_BYTES: usize = 256 * 1024;

/// How a blob travels between the client and this server.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Identity,
    Zstd,
}

impl Codec {
    #[lore_macro::test_pub]
    fn from_proto(value: i32) -> Result<Self, String> {
        match compressor::Value::try_from(value) {
            Ok(compressor::Value::Identity) => Ok(Codec::Identity),
            Ok(compressor::Value::Zstd) => Ok(Codec::Zstd),
            _ => Err(format!(
                "compressor {value} is not supported; this server offers zstd"
            )),
        }
    }

    fn to_proto(self) -> i32 {
        match self {
            Codec::Identity => compressor::Value::Identity as i32,
            Codec::Zstd => compressor::Value::Zstd as i32,
        }
    }

    #[lore_macro::test_pub]
    fn encode(self, data: Vec<u8>) -> Result<Vec<u8>, Status> {
        match self {
            Codec::Identity => Ok(data),
            Codec::Zstd => zstd::bulk::compress(&data, ZSTD_LEVEL).map_err(internal),
        }
    }

    /// [`Codec::encode`], on the blocking pool when the blob is large enough to matter.
    #[lore_macro::test_pub]
    async fn encode_async(self, data: Vec<u8>) -> Result<Vec<u8>, Status> {
        if self == Codec::Identity || data.len() < OFF_RUNTIME_BYTES {
            return self.encode(data);
        }
        rbe_lore::spawn_blocking(move || self.encode(data))
            .await
            .map_err(internal)?
    }

    /// [`Codec::decode`] into an owned blob, and its SHA-256: on the blocking pool when the blob
    /// is large, in one hop for both.
    #[lore_macro::test_pub]
    async fn decode_and_hash(self, data: Vec<u8>, size: i64) -> Result<(Vec<u8>, String), Status> {
        let large = data.len().max(size.max(0) as usize) >= OFF_RUNTIME_BYTES;
        let work = move || -> Result<(Vec<u8>, String), Status> {
            let blob = match self {
                Codec::Identity => data,
                Codec::Zstd => self
                    .decode(&data, size)
                    .map_err(Status::invalid_argument)?
                    .into_owned(),
            };
            let hash = digest::sha256_hex(&blob);
            Ok((blob, hash))
        };
        if !large {
            return work();
        }
        rbe_lore::spawn_blocking(work).await.map_err(internal)?
    }

    /// Turn what the client sent back into the blob. Decompression is capped at the size the
    /// digest declares, so a frame that would expand further fails instead of allocating it.
    #[lore_macro::test_pub]
    fn decode(self, data: &[u8], size: i64) -> Result<Cow<'_, [u8]>, String> {
        match self {
            Codec::Identity => Ok(Cow::Borrowed(data)),
            Codec::Zstd => zstd::bulk::decompress(data, size.max(0) as usize)
                .map(Cow::Owned)
                .map_err(|e| format!("zstd data does not decompress to {size} bytes: {e}")),
        }
    }
}

/// One `<hash> <size>` line for every blob a client wrote, behind `--log-cas-writes`.
///
/// The counters say how much bazel uploaded; this says what. Matching each digest against the
/// SHA-256s of the workspace and of bazel's external repositories attributes every upload to a
/// file, and whatever matches nothing is bazel's own per-action metadata (`SOURCES.md` §7).
/// Only this process's writes are logged: those are bazel's uploads, since a worker writes its
/// outputs through its own store.
pub struct WriteLog(Mutex<std::io::BufWriter<std::fs::File>>);

impl WriteLog {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self(Mutex::new(std::io::BufWriter::new(
            std::fs::File::create(path)?,
        ))))
    }

    /// Flushed per batch, so the file is complete whenever the server is stopped. A failure to
    /// log is reported but never fails the upload it describes.
    fn record<'a>(&self, blobs: impl IntoIterator<Item = (&'a str, i64)>) {
        let mut w = self.0.lock().unwrap();
        let result = blobs
            .into_iter()
            .try_for_each(|(hash, size)| writeln!(w, "{hash} {size}"))
            .and_then(|()| w.flush());
        if let Err(err) = result {
            tracing::warn!("--log-cas-writes: {err}");
        }
    }
}

pub struct CasService {
    store: Arc<LoreBlobStore>,
    writes: Option<Arc<WriteLog>>,
}

impl CasService {
    pub fn new(store: Arc<LoreBlobStore>, writes: Option<Arc<WriteLog>>) -> Self {
        Self { store, writes }
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for CasService {
    async fn find_missing_blobs(
        &self,
        request: Request<rbe_proto::reapi::FindMissingBlobsRequest>,
    ) -> Result<Response<rbe_proto::reapi::FindMissingBlobsResponse>, Status> {
        let req = request.into_inner();

        // The empty blob is always present by definition and must never be reported missing.
        let (probe, probe_idx): (Vec<_>, Vec<_>) = req
            .blob_digests
            .iter()
            .enumerate()
            .filter(|(_, d)| !is_empty_digest(d))
            .map(|(i, d)| (key_of(d), i))
            .unzip();

        let present = self
            .store
            .exists_many(Ns::Cas, &probe)
            .await
            .map_err(internal)?;

        let missing = probe_idx
            .iter()
            .zip(present)
            .filter(|(_, found)| !*found)
            .map(|(i, _)| req.blob_digests[*i].clone())
            .collect();

        Ok(Response::new(rbe_proto::reapi::FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let req = request.into_inner();

        let mut responses = Vec::with_capacity(req.requests.len());
        let mut to_store: Vec<(String, i64, Cow<'_, [u8]>)> =
            Vec::with_capacity(req.requests.len());
        let wire: usize = req.requests.iter().map(|r| r.data.len()).sum();
        self.store
            .stats
            .wire_write_bytes
            .fetch_add(wire as u64, Ordering::Relaxed);

        for r in &req.requests {
            let Some(d) = r.digest.as_ref() else {
                responses.push(batch_update_blobs_response::Response {
                    digest: None,
                    status: Some(err_status(tonic::Code::InvalidArgument, "missing digest")),
                });
                continue;
            };
            let data = match Codec::from_proto(r.compressor)
                .and_then(|codec| codec.decode(&r.data, d.size_bytes))
            {
                Ok(data) => data,
                Err(message) => {
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(d.clone()),
                        status: Some(err_status(tonic::Code::InvalidArgument, message)),
                    });
                    continue;
                }
            };
            // Verify rather than trust: a wrong digest here would poison the cache for every
            // client that later resolves it, and the check is one hash of data already in hand.
            if d.size_bytes as usize != data.len() {
                responses.push(batch_update_blobs_response::Response {
                    digest: Some(d.clone()),
                    status: Some(err_status(
                        tonic::Code::InvalidArgument,
                        format!("size {} != {} bytes of data", d.size_bytes, data.len()),
                    )),
                });
                continue;
            }
            let actual = digest::sha256_hex(&data);
            if actual != d.hash {
                responses.push(batch_update_blobs_response::Response {
                    digest: Some(d.clone()),
                    status: Some(err_status(
                        tonic::Code::InvalidArgument,
                        format!("digest mismatch: content hashes to {actual}"),
                    )),
                });
                continue;
            }

            responses.push(batch_update_blobs_response::Response {
                digest: Some(d.clone()),
                status: Some(ok_status()),
            });
            if !is_empty_digest(d) {
                to_store.push((d.hash.clone(), d.size_bytes, data));
            }
        }

        let batch: Vec<(String, i64, &[u8])> = to_store
            .iter()
            .map(|(hash, size, data)| (hash.clone(), *size, data.as_ref()))
            .collect();
        self.store
            .put_many(Ns::Cas, &batch)
            .await
            .map_err(internal)?;
        if let Some(log) = &self.writes {
            log.record(
                to_store
                    .iter()
                    .map(|(hash, size, _)| (hash.as_str(), *size)),
            );
        }

        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let req = request.into_inner();
        // Answer in zstd whenever the client accepts it: what the cache holds is mostly object
        // files and binaries, which compress well.
        let codec = if req
            .acceptable_compressors
            .contains(&(compressor::Value::Zstd as i32))
        {
            Codec::Zstd
        } else {
            Codec::Identity
        };

        let keys: Vec<_> = req
            .digests
            .iter()
            .filter(|d| !is_empty_digest(d))
            .map(key_of)
            .collect();
        // Lore hands zstd back as it stores it, so nothing is compressed here.
        let delivery = match codec {
            Codec::Identity => Delivery::Content,
            Codec::Zstd => Delivery::Zstd,
        };
        let mut fetched = self
            .store
            .get_many_as(Ns::Cas, &keys, delivery)
            .await
            .map_err(internal)?
            .into_iter();

        let mut responses = Vec::with_capacity(req.digests.len());
        for d in &req.digests {
            if is_empty_digest(d) {
                responses.push(batch_read_blobs_response::Response {
                    digest: Some(d.clone()),
                    data: Vec::new(),
                    compressor: 0,
                    status: Some(ok_status()),
                });
                continue;
            }
            let slot = fetched.next().flatten();
            responses.push(match slot {
                Some(data) => {
                    self.store
                        .stats
                        .wire_read_bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    batch_read_blobs_response::Response {
                        digest: Some(d.clone()),
                        data,
                        compressor: codec.to_proto(),
                        status: Some(ok_status()),
                    }
                }
                None => batch_read_blobs_response::Response {
                    digest: Some(d.clone()),
                    data: Vec::new(),
                    compressor: 0,
                    status: Some(err_status(tonic::Code::NotFound, "blob not in cache")),
                },
            });
        }

        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = Pin<Box<dyn Stream<Item = Result<GetTreeResponse, Status>> + Send>>;

    /// Breadth-first walk of a `Directory` tree, all pages in one response. Bazel does not use
    /// this during a build (it materialises trees from `ActionResult`), but other REAPI clients
    /// and debugging tools do.
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        let req = request.into_inner();
        let root = req
            .root_digest
            .ok_or_else(|| Status::invalid_argument("missing root_digest"))?;

        let mut out: Vec<Directory> = Vec::new();
        let mut frontier = vec![root];
        let mut seen = std::collections::HashSet::new();

        while !frontier.is_empty() {
            let keys: Vec<_> = frontier
                .iter()
                .filter(|d| !is_empty_digest(d))
                .map(key_of)
                .collect();
            let blobs = self
                .store
                .get_many(Ns::Cas, &keys)
                .await
                .map_err(internal)?;

            let mut next = Vec::new();
            let mut blobs = blobs.into_iter();
            for d in &frontier {
                let dir = if is_empty_digest(d) {
                    Directory::default()
                } else {
                    let Some(bytes) = blobs.next().flatten() else {
                        return Err(Status::not_found(format!(
                            "directory {} not in cache",
                            digest::fmt(d)
                        )));
                    };
                    <Directory as prost::Message>::decode(bytes.as_slice())
                        .map_err(|e| Status::internal(format!("decoding Directory: {e}")))?
                };
                for child in &dir.directories {
                    if let Some(cd) = child.digest.as_ref()
                        && seen.insert((cd.hash.clone(), cd.size_bytes))
                    {
                        next.push(cd.clone());
                    }
                }
                out.push(dir);
            }
            frontier = next;
        }

        let stream = futures::stream::once(async move {
            Ok(GetTreeResponse {
                directories: out,
                next_page_token: String::new(),
            })
        });
        Ok(Response::new(Box::pin(stream)))
    }

    /// Blob splitting (REAPI 2.4) lets a client fetch a large blob as content-defined chunks.
    /// Not advertised in `CacheCapabilities`, so no conforming client will call these.
    async fn split_blob(
        &self,
        _request: Request<rbe_proto::reapi::SplitBlobRequest>,
    ) -> Result<Response<rbe_proto::reapi::SplitBlobResponse>, Status> {
        Err(Status::unimplemented("blob splitting is not supported"))
    }

    async fn splice_blob(
        &self,
        _request: Request<rbe_proto::reapi::SpliceBlobRequest>,
    ) -> Result<Response<rbe_proto::reapi::SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("blob splicing is not supported"))
    }
}

/// A parsed ByteStream resource name.
#[lore_macro::test_pub]
#[derive(Debug)]
struct Resource {
    hash: String,
    size: i64,
    codec: Codec,
}

/// Parse both the read form (`{instance/}blobs/{hash}/{size}`) and the write form
/// (`{instance/}uploads/{uuid}/blobs/{hash}/{size}{/metadata}`), and their compressed
/// variants, where `blobs` is `compressed-blobs/{compressor}` and the hash and size are still
/// those of the uncompressed blob. An optional `{digest_function}` segment before the hash is
/// tolerated. The instance name is accepted and ignored -- this server has exactly one
/// namespace.
#[lore_macro::test_pub]
fn parse_resource(name: &str) -> Result<Resource, Status> {
    let parts: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();

    let anchor = parts
        .iter()
        .position(|p| *p == "blobs" || *p == "compressed-blobs")
        .ok_or_else(|| {
            Status::invalid_argument(format!("resource name has no blobs segment: {name}"))
        })?;
    let mut rest = &parts[anchor + 1..];
    let mut codec = Codec::Identity;
    if parts[anchor] == "compressed-blobs" {
        match rest.first() {
            Some(&"zstd") => codec = Codec::Zstd,
            Some(other) => {
                return Err(Status::unimplemented(format!(
                    "compressor {other} is not supported; this server offers zstd"
                )));
            }
            None => {
                return Err(Status::invalid_argument(format!(
                    "resource name has no compressor: {name}"
                )));
            }
        }
        rest = &rest[1..];
    }
    // An explicit digest-function segment is optional. A SHA-256 hash is 64 hex chars, which no
    // digest-function name is, so the two are unambiguous.
    if let Some(first) = rest.first()
        && !(first.len() == 64 && first.chars().all(|c| c.is_ascii_hexdigit()))
    {
        if !first.eq_ignore_ascii_case("sha256") {
            return Err(Status::unimplemented(format!(
                "unsupported digest function {first}"
            )));
        }
        rest = &rest[1..];
    }

    let (hash, size) = match rest {
        [hash, size, ..] => (*hash, *size),
        _ => {
            return Err(Status::invalid_argument(format!(
                "resource name is missing hash/size: {name}"
            )));
        }
    };
    let size: i64 = size
        .parse()
        .map_err(|_| Status::invalid_argument(format!("bad size in resource name: {name}")))?;

    Ok(Resource {
        hash: hash.to_string(),
        size,
        codec,
    })
}

pub struct ByteStreamService {
    store: Arc<LoreBlobStore>,
    writes: Option<Arc<WriteLog>>,
}

impl ByteStreamService {
    pub fn new(store: Arc<LoreBlobStore>, writes: Option<Arc<WriteLog>>) -> Self {
        Self { store, writes }
    }

    /// The bytes of a read as gRPC messages of at most [`READ_CHUNK_BYTES`], each a view onto
    /// `wire` rather than a copy, counted as sent to the client.
    fn read_response(&self, wire: bytes::Bytes) -> Response<ReadStream> {
        self.store
            .stats
            .wire_read_bytes
            .fetch_add(wire.len() as u64, Ordering::Relaxed);
        let frames: Vec<_> = (0..wire.len())
            .step_by(READ_CHUNK_BYTES)
            .map(|at| {
                Ok(ReadResponse {
                    data: wire.slice(at..(at + READ_CHUNK_BYTES).min(wire.len())),
                })
            })
            .collect();
        Response::new(Box::pin(futures::stream::iter(frames)))
    }
}

type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

#[tonic::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = ReadStream;

    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let req = request.into_inner();
        let res = parse_resource(&req.resource_name)?;

        // A whole blob asked for in zstd, the usual compressed read, is the stream of frames Lore
        // already stores, passed on as it is: Lore compressed it once when it was written, and the
        // client expands it once. Only a read of part of a blob is cut from the content and
        // compressed here.
        if res.codec == Codec::Zstd && req.read_offset == 0 && req.read_limit == 0 {
            let wire = if res.size == 0 && res.hash == EMPTY_SHA256 {
                EMPTY_ZSTD_FRAME.to_vec()
            } else {
                self.store
                    .get_as(Ns::Cas, &res.hash, res.size, Delivery::Zstd)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| {
                        Status::not_found(format!("blob {}/{} not in cache", res.hash, res.size))
                    })?
            };
            return Ok(self.read_response(bytes::Bytes::from(wire)));
        }

        let data = if res.size == 0 && res.hash == EMPTY_SHA256 {
            Vec::new()
        } else {
            self.store
                .get(Ns::Cas, &res.hash, res.size)
                .await
                .map_err(internal)?
                .ok_or_else(|| {
                    Status::not_found(format!("blob {}/{} not in cache", res.hash, res.size))
                })?
        };

        let mut data = data;
        let start = req.read_offset.max(0) as usize;
        if start > data.len() {
            return Err(Status::out_of_range(format!(
                "read_offset {start} beyond blob size {}",
                data.len()
            )));
        }
        if req.read_limit > 0 {
            data.truncate(start + req.read_limit as usize);
        }
        // Whole blobs are the rule, and they are not copied here: only a read starting part way
        // in moves bytes. For a compressed read the offset and limit refer to the uncompressed
        // blob, so the requested range is cut first and compressed on its own.
        if start > 0 {
            data.drain(..start);
        }
        let wire = bytes::Bytes::from(res.codec.encode_async(data).await?);
        Ok(self.read_response(wire))
    }

    /// Accumulate the stream and store it once, after checking the content against the digest
    /// in the resource name. Buffering the whole blob is the prototype's simplification: it
    /// bounds an upload's memory by the largest single output rather than streaming into Lore
    /// fragment by fragment.
    ///
    /// A compressed upload arrives as zstd data and is decompressed before the check. Its write
    /// offsets count the compressed bytes received so far, as the REAPI specifies, and so does
    /// the committed size in the reply.
    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut stream = request.into_inner();
        let mut resource: Option<Resource> = None;
        let mut buf: Vec<u8> = Vec::new();
        let mut saw_finish = false;

        while let Some(msg) = stream.message().await? {
            if resource.is_none() && !msg.resource_name.is_empty() {
                let res = parse_resource(&msg.resource_name)?;
                buf.reserve(res.size.max(0) as usize);
                resource = Some(res);
            }
            if msg.write_offset != buf.len() as i64 {
                return Err(Status::invalid_argument(format!(
                    "write_offset {} does not continue from {} bytes received",
                    msg.write_offset,
                    buf.len()
                )));
            }
            buf.extend_from_slice(&msg.data);
            if msg.finish_write {
                saw_finish = true;
                break;
            }
        }

        let res = resource
            .ok_or_else(|| Status::invalid_argument("no resource_name in the write stream"))?;
        if !saw_finish {
            return Err(Status::invalid_argument(
                "write stream ended without finish_write",
            ));
        }
        let received = buf.len() as i64;
        self.store
            .stats
            .wire_write_bytes
            .fetch_add(received as u64, Ordering::Relaxed);
        let (buf, actual) = res.codec.decode_and_hash(buf, res.size).await?;
        if buf.len() as i64 != res.size {
            return Err(Status::invalid_argument(format!(
                "declared size {} but received {} bytes",
                res.size,
                buf.len()
            )));
        }
        if actual != res.hash {
            return Err(Status::invalid_argument(format!(
                "digest mismatch: content hashes to {actual}, resource says {}",
                res.hash
            )));
        }

        if !(res.size == 0 && res.hash == EMPTY_SHA256) {
            self.store
                .put(Ns::Cas, &res.hash, res.size, &buf)
                .await
                .map_err(internal)?;
            if let Some(log) = &self.writes {
                log.record([(res.hash.as_str(), res.size)]);
            }
        }

        Ok(Response::new(WriteResponse {
            committed_size: match res.codec {
                Codec::Identity => res.size,
                Codec::Zstd => received,
            },
        }))
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let req = request.into_inner();
        let res = parse_resource(&req.resource_name)?;

        // Uploads are not resumable here -- they are buffered in memory and committed as a
        // unit, so the only two answers are "already in the cache" and "start over".
        let present = res.size == 0 && res.hash == EMPTY_SHA256
            || *self
                .store
                .exists_many(Ns::Cas, &[(res.hash.clone(), res.size)])
                .await
                .map_err(internal)?
                .first()
                .unwrap_or(&false);

        // A finished compressed upload has no single meaningful size in compressed bytes (it
        // depends on how the client compressed it); the REAPI uses -1 for "already present".
        Ok(Response::new(QueryWriteStatusResponse {
            committed_size: match (present, res.codec) {
                (false, _) => 0,
                (true, Codec::Identity) => res.size,
                (true, Codec::Zstd) => -1,
            },
            complete: present,
        }))
    }
}

#[derive(Default)]
pub struct CapabilitiesService;

#[tonic::async_trait]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        Ok(Response::new(ServerCapabilities {
            cache_capabilities: Some(CacheCapabilities {
                digest_functions: vec![digest_function::Value::Sha256 as i32],
                action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                cache_priority_capabilities: None,
                max_batch_total_size_bytes: MAX_BATCH_TOTAL_BYTES,
                symlink_absolute_path_strategy: symlink_absolute_path_strategy::Value::Allowed
                    as i32,
                // zstd through ByteStream and the batch calls alike. Bazel compresses only when
                // asked to (--remote_cache_compression), so uncompressed clients are unaffected.
                supported_compressors: vec![compressor::Value::Zstd as i32],
                supported_batch_update_compressors: vec![compressor::Value::Zstd as i32],
                // Blob splitting/splicing and CDC parameters are left at their defaults, which
                // is how a server says it does not offer them.
                ..Default::default()
            }),
            execution_capabilities: Some(ExecutionCapabilities {
                digest_function: digest_function::Value::Sha256 as i32,
                exec_enabled: true,
                execution_priority_capabilities: None,
                supported_node_properties: Vec::new(),
                digest_functions: vec![digest_function::Value::Sha256 as i32],
            }),
            deprecated_api_version: None,
            low_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
            }),
            high_api_version: Some(SemVer {
                major: 2,
                minor: 3,
                patch: 0,
                prerelease: String::new(),
            }),
        }))
    }
}

/// Helper shared with the worker-facing paths: read one blob or fail with NOT_FOUND.
pub async fn read_blob(store: &LoreBlobStore, d: &Digest) -> Result<Vec<u8>, Status> {
    if is_empty_digest(d) {
        return Ok(Vec::new());
    }
    store
        .get(Ns::Cas, &d.hash, d.size_bytes)
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("blob {} not in cache", digest::fmt(d))))
}
