// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The Lore-backed blob store that the server and the workers both sit on.
//!
//! Bazel's remote cache is two string-keyed blob maps: the CAS (key = content digest) and the
//! Action Cache (key = action digest -> serialised `ActionResult`). Lore's foreign-key storage
//! API is exactly that shape, so both map onto one pair of calls:
//!
//! ```text
//!   put(key, blob): put_resolved(H(key), blob)   // stores content, then publishes the key
//!   get(key):       get_resolved(H(key))         // resolves the key, returns the content
//! ```
//!
//! Blob content that is already a file, or is about to become one, uses the path-taking form of
//! the same pair instead -- `put_file_resolved` / `get_file_resolved`. Those never assemble the
//! content in this process: a large file chunks straight off disk on the way in and is written
//! leaf by leaf at its own offset on the way out. An input root of a few hundred megabytes costs
//! a fragment of residency rather than all of it, which is what lets a worker materialise one
//! per slot.
//!
//! Tiering is the API's job, not ours (this is the part the sccache backend proved out):
//!   * `put_resolved` always writes the local store and, with `remote_write`, also publishes to
//!     the upstream lore-server. Content is stored *before* the key names it, so a key never
//!     resolves to something absent.
//!   * `get_resolved` reads local first and falls back to the upstream on a miss, caching what
//!     it fetches (`local_cache`), so an upstream hit populates the local tier -- both the blob
//!     and the key mapping -- and the next read for that key is local.
//!
//! So there are two tiers, configured rather than implemented here:
//!   * local (`--lore-repo`, always present): a disk-backed Lore repository opened in-process.
//!   * upstream (`--lore-server`, optional): the shared cache every executor sees.
//!
//! Every op is batched: one Lore call carries N items, each with a caller-chosen `id` echoed
//! back on its events, which is what lets `FindMissingBlobs` over 2000 inputs be one call
//! instead of 2000.
//!
//! Every op here is also delegable: each goes through Lore's `dispatch_call`, so setting
//! `LORE_USE_SERVICE` routes it over IPC to a Lore service process instead of running it in
//! this one, with no change to these call sites. That is only true because the write path is
//! `put_file_resolved` and not `put_resolved`: the latter carries a `LoreBytes` view into caller
//! memory in its *args*, which has no cross-process representation, and fails as a dropped
//! connection rather than an error. `put_many` is therefore for a process that owns its store,
//! and `put_file_many` for one that may not.

pub mod digest;
pub mod workspace;
pub mod zstd_frames;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use lore::repository::LoreVfsType;
use lore::storage::get_file_resolved::LoreStorageGetFileResolvedArgs;
use lore::storage::get_file_resolved::LoreStorageGetFileResolvedItem;
use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
use lore::storage::get_resolved::LoreStorageGetResolvedItem;
use lore::storage::handle::LoreStore as LoreStoreHandle;
use lore::storage::mutable_load::LoreStorageMutableLoadArgs;
use lore::storage::mutable_load::LoreStorageMutableLoadItem;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::LoreStorageRemoteConfig;
use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
use lore::storage::put_resolved::LoreStoragePutResolvedItem;
use lore::storage::{self};
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::LoreSharedStoreMode;

/// Runs `work` on the blocking pool of the runtime the calling task runs on.
///
/// Not `lore_spawn_blocking!`, which moves the work onto Lore's core runtime and its thread
/// budget: the compressing, hashing and file layout done here stays on the runtime that serves the
/// requests needing it.
#[allow(clippy::disallowed_methods)]
pub fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(work)
}

/// Which partition an entry lives in. Both hold the same `cas:` keys over the same content, and
/// differ only in lifetime.
///
/// Keeping toolchains out of the build cache is what lets the build cache be capped and
/// garbage-collected without ever evicting a compiler. It also decouples them: a toolchain is
/// seeded once per version and shared by every project in every repository, whereas the build
/// cache belongs to the builds that filled it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// Everything a build produces or consumes. Capped, GC on.
    BuildCache,
    /// The durable partition: content placed deliberately rather than produced by a build.
    /// Seeded toolchains, and files published from Lore repositories by `lore-rbe-index
    /// publish`. GC off. Its keys are content-addressed, so they stay valid forever.
    Toolchain,
}

/// Dedicated, non-zero partitions so RBE entries can never collide with real repository data.
/// Mutable ops reject the default (zero) partition.
#[lore_macro::test_pub]
fn rbe_partition(part: Part) -> Partition {
    let seed: &[u8] = match part {
        Part::BuildCache => b"bazel-rbe.v1.partition",
        Part::Toolchain => b"bazel-rbe.v1.toolchains",
    };
    let mut p = Partition::default();
    p.data_mut()
        .copy_from_slice(&Hash::hash_buffer(seed).data()[..16]);
    p
}

/// One fixed context for every entry: `get_resolved` must read a key at the same context
/// `put_resolved` published it under.
fn rbe_context() -> Context {
    Context::default()
}

/// `correlation_id` must be stable across ops -- Lore keys its storage `SessionPool` on
/// `(repository, correlation_id)` and mints a fresh UUID whenever the field is empty, so
/// leaving it unset makes every op miss the pool and pay an extra `session_start` round trip.
fn globals(correlation_id: &str) -> LoreGlobalArgs {
    LoreGlobalArgs {
        correlation_id: correlation_id.into(),
        ..Default::default()
    }
}

/// Same, but routed at the upstream instead of the local store. Used only by the existence
/// check, which -- unlike `get_resolved` -- does not do its own tiering.
fn remote_globals(correlation_id: &str) -> LoreGlobalArgs {
    LoreGlobalArgs {
        correlation_id: correlation_id.into(),
        remote: 1,
        ..Default::default()
    }
}

/// Items per `get_file_resolved` / `put_file_resolved` call. Both run one task per item and, with
/// an upstream, one request per item, so an unsplit input root of a few thousand files would
/// arrive at the lore-server as a burst deep enough to be shed by its per-stream admission limit.
/// Enough to keep the link busy, bounded enough to stay under it.
const MAX_FILE_ITEMS_PER_CALL: usize = 256;

fn prof_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LORE_PROF").is_some())
}

/// Cache-op counters, reported on shutdown and by `--stats-interval`. Cheap enough to always
/// keep: the interesting question in a benchmark is always "how many of these were hits".
#[derive(Default)]
pub struct Stats {
    pub cas_get_hit: AtomicU64,
    pub cas_get_miss: AtomicU64,
    pub cas_get_bytes: AtomicU64,
    pub cas_put: AtomicU64,
    pub cas_put_bytes: AtomicU64,
    pub ac_get_hit: AtomicU64,
    pub ac_get_miss: AtomicU64,
    /// Lookups that found an entry but could not use it, or could not read one at all. Distinct
    /// from a miss: it means warm builds are quietly running cold.
    pub ac_degraded: AtomicU64,
    pub ac_put: AtomicU64,
    pub exists_present: AtomicU64,
    pub exists_absent: AtomicU64,
    pub actions_executed: AtomicU64,
    pub actions_ac_hit: AtomicU64,
    /// Wall time workers spent materialising input roots, over how many actions. This is the
    /// number that scales with distance to the shared store, and so the one the executor-local
    /// tier exists to hold down; wall-clock alone hides it behind the compilers.
    pub input_fetch_ms: AtomicU64,
    pub input_fetch_actions: AtomicU64,
    /// Blob content exchanged with REAPI clients, as it crossed the wire: compressed when the
    /// client asked for zstd, raw otherwise. Only the scheduler serves clients, so on a worker
    /// these stay zero and the segment is left out.
    pub wire_read_bytes: AtomicU64,
    pub wire_write_bytes: AtomicU64,
}

impl Stats {
    pub fn render(&self) -> String {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        // Only a worker materialises anything, so the segment is omitted rather than reported as
        // zero on a process that never could.
        let fetch = match l(&self.input_fetch_actions) {
            0 => String::new(),
            actions => format!(
                " | input fetch: {:.1} s over {actions} actions",
                l(&self.input_fetch_ms) as f64 / 1000.0
            ),
        };
        let mib = |a: &AtomicU64| l(a) as f64 / (1024.0 * 1024.0);
        let wire = match (l(&self.wire_read_bytes), l(&self.wire_write_bytes)) {
            (0, 0) => String::new(),
            _ => format!(
                " | wire: {:.1} MiB to clients, {:.1} MiB from clients",
                mib(&self.wire_read_bytes),
                mib(&self.wire_write_bytes)
            ),
        };
        format!(
            "ac: {} hits / {} misses / {} degraded, {} writes | cas: {} reads ({} hits, {} \
             misses, {:.1} MiB), {} writes ({:.1} MiB) | find_missing: {} present / {} absent | \
             actions: {} executed, {} served from cache{}{}",
            l(&self.ac_get_hit),
            l(&self.ac_get_miss),
            l(&self.ac_degraded),
            l(&self.ac_put),
            l(&self.cas_get_hit) + l(&self.cas_get_miss),
            l(&self.cas_get_hit),
            l(&self.cas_get_miss),
            l(&self.cas_get_bytes) as f64 / (1024.0 * 1024.0),
            l(&self.cas_put),
            l(&self.cas_put_bytes) as f64 / (1024.0 * 1024.0),
            l(&self.exists_present),
            l(&self.exists_absent),
            l(&self.actions_executed),
            l(&self.actions_ac_hit),
            fetch,
            wire,
        )
    }
}

/// How a read hands content back.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The content itself.
    Content,
    /// A zstd stream of the content, made here of the leaves Lore delivers as it stores them: a
    /// leaf stored with zstd, which is Lore's default, is already a zstd frame and is passed on
    /// rather than expanded and compressed again, and a leaf stored uncompressed travels in raw
    /// blocks. A leaf stored with any other codec fails the read; see [`zstd_frames`].
    Zstd,
}

/// The zstd frame of empty content: a single segment stating size 0, then one empty raw block.
pub const EMPTY_ZSTD_FRAME: [u8; 9] = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x00, 0x01, 0x00, 0x00];

/// What the local tier keeps of the content that passes through it.
///
/// With an upstream configured, content is always published there; this only decides whether a
/// second copy also stays on this machine. That copy is the executor-side cache when the
/// upstream is across a network. It is pure duplication when the upstream runs on the same
/// machine, as the scheduler's does, and for content something else already keeps, as a
/// worker's staging directory keeps its inputs and outputs.
///
/// Off is what Lore calls `local_cache = 0`: a write whose content the upstream already holds
/// durably leaves only fragment metadata locally, and a read does not keep what it fetched.
/// Without an upstream the local tier is the only copy, and Lore keeps it regardless.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LocalCache {
    /// Content moved through memory: `Action`, `Command` and `Directory` messages, action
    /// results, and every blob the scheduler serves bazel.
    pub blobs: bool,
    /// Content moved by path: a worker's inputs and outputs.
    pub files: bool,
}

impl Default for LocalCache {
    fn default() -> Self {
        Self {
            blobs: true,
            files: true,
        }
    }
}

/// Which of the two key namespaces an entry lives in. Both share one partition; the prefix in
/// the hashed key keeps them apart, so an action digest and a blob digest that happen to be
/// equal (they can be -- an `Action` message is itself a CAS blob) do not collide.
#[derive(Copy, Clone, Debug)]
pub enum Ns {
    /// Content-addressable storage: digest -> blob bytes.
    Cas,
    /// Action cache: action digest -> serialised `ActionResult`.
    Ac,
}

impl Ns {
    fn prefix(self) -> &'static str {
        match self {
            Ns::Cas => "cas",
            Ns::Ac => "ac",
        }
    }
}

/// `"<ns>:<digest hash>:<size>"`, hashed, is the Lore mutable key. The same key in either
/// partition names the same content, which is what lets a read fall through from one to the
/// other without knowing anything about the path it came from.
///
/// This is a storage format: every entry already published, seeded toolchains included, is
/// reachable only through it.
#[lore_macro::test_pub]
fn key(ns: Ns, hash: &str, size: i64) -> Hash {
    Hash::hash_buffer(format!("{}:{hash}:{size}", ns.prefix()).as_bytes())
}

/// Whether `(hash, size)` names the empty blob in `ns`, which is never stored and so is produced
/// rather than read. Only a CAS digest can: an action digest is a key, not content, and an empty
/// one names no stored result.
#[lore_macro::test_pub]
fn is_empty_content(ns: Ns, hash: &str, size: i64) -> bool {
    matches!(ns, Ns::Cas) && digest::is_empty_blob(hash, size)
}

pub struct LoreBlobStore {
    handle: LoreStoreHandle,
    build_cache: Partition,
    toolchain: Partition,
    context: Context,
    /// True when an upstream lore-server is configured; drives `remote_write` on puts and the
    /// upstream leg of the existence check.
    upstream: bool,
    location: String,
    correlation_id: String,
    local_cache: LocalCache,
    pub stats: Stats,
}

impl LoreBlobStore {
    /// Open the store. The disk repo at `repo_path` is always the local tier (created on first
    /// use); `server_url` optionally attaches an upstream lore-server as the shared tier.
    /// `cache_target_bytes` caps the local tier and enables Lore's GC; 0 disables GC, which is
    /// what a benchmark wants (nothing can be evicted mid-measurement).
    pub async fn open(
        repo_path: &str,
        cache_target_bytes: u64,
        server_url: Option<&str>,
    ) -> Result<Self> {
        Self::ensure_repo(repo_path).await?;
        Self::open_handle(repo_path, cache_target_bytes, server_url).await
    }

    /// Open the store on an existing Lore workspace instead of a repository of our own, with
    /// `server_url` as the upstream. Nothing is created or reconfigured.
    ///
    /// This is how content already in a Lore repository gets published under CAS keys without
    /// being sent again: the handle shares the workspace's local store, so a write finds the
    /// workspace's fragments there, already stored on the server under the repository's
    /// partition, and Lore copies them into ours server-side instead of uploading them.
    ///
    /// A path that is not a workspace is an error rather than a new repository, which is what
    /// `open` would make of it.
    pub async fn open_checkout(checkout: &str, server_url: &str) -> Result<Self> {
        if !Path::new(checkout).join(".lore").is_dir() {
            bail!("{checkout} is not a Lore workspace: it has no .lore directory");
        }
        Self::open_handle(checkout, 0, Some(server_url)).await
    }

    async fn open_handle(
        repo_path: &str,
        cache_target_bytes: u64,
        server_url: Option<&str>,
    ) -> Result<Self> {
        let upstream = server_url.is_some();
        let correlation_id = format!("lore-rbe-{}", uuid::Uuid::new_v4());

        let captured: Arc<Mutex<Option<u64>>> = Default::default();
        let cap = captured.clone();
        let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
            if let LoreEvent::StorageOpened(d) = ev {
                *cap.lock().unwrap() = Some(d.handle_id);
            }
        }));

        let args = LoreStorageOpenArgs {
            repository_path: repo_path.into(),
            in_memory: 0,
            has_remote_config: upstream as u8,
            remote_config: LoreStorageRemoteConfig {
                remote_url: server_url.unwrap_or_default().into(),
            },
            // Keep the re-hash on read. A wrong blob served under a digest is a wrong build for
            // every client that resolves it, and nothing else here would catch one.
            skip_verify: 0,
            cache_target_bytes,
            cache_target_fragments: 0,
        };

        let status = storage::open::open(globals(&correlation_id), args, cb).await;
        let handle_id = captured
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("lore storage open failed (status={status})"))?;

        let location = match server_url {
            Some(url) => format!("{repo_path} + upstream {url}"),
            None => repo_path.to_string(),
        };

        Ok(Self {
            handle: LoreStoreHandle { handle_id },
            build_cache: rbe_partition(Part::BuildCache),
            toolchain: rbe_partition(Part::Toolchain),
            context: rbe_context(),
            upstream,
            location,
            correlation_id,
            local_cache: LocalCache::default(),
            stats: Stats::default(),
        })
    }

    /// Set what the local tier keeps; see [`LocalCache`]. The default keeps everything.
    pub fn with_local_cache(mut self, policy: LocalCache) -> Self {
        self.local_cache = policy;
        self
    }

    pub fn location(&self) -> &str {
        &self.location
    }

    pub fn has_upstream(&self) -> bool {
        self.upstream
    }

    fn partition(&self, part: Part) -> Partition {
        match part {
            Part::BuildCache => self.build_cache,
            Part::Toolchain => self.toolchain,
        }
    }

    /// Partitions a read of `ns` must consult, in order.
    ///
    /// Only CAS content is ever seeded, so only CAS reads fall through. An `ActionResult` names
    /// a specific build's outputs and has no meaning in a partition shared across projects.
    #[lore_macro::test_pub]
    fn read_order(ns: Ns) -> &'static [Part] {
        match ns {
            Ns::Cas => &[Part::BuildCache, Part::Toolchain],
            Ns::Ac => &[Part::BuildCache],
        }
    }

    async fn ensure_repo(repo_path: &str) -> Result<()> {
        if std::path::Path::new(repo_path).join(".lore").is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(repo_path)
            .map_err(|e| anyhow!("creating lore repo dir {repo_path}: {e}"))?;

        let status = lore::repository::create(
            LoreGlobalArgs {
                repository_path: repo_path.into(),
                offline: 1,
                ..Default::default()
            },
            lore::repository::LoreRepositoryCreateArgs {
                repository_url: "lore://localhost/bazel-rbe-cache".into(),
                description: LoreString::default(),
                id: LoreString::default(),
                vfs: LoreVfsType::None,
                // Never the machine's shared store, whatever its global setting. The local tier
                // has to live in this directory: wiping it is what makes a scheduler or an
                // executor clean, and each worker process is meant to have its own. Inherited, it
                // landed in a machine-wide store no wipe reaches, and the next "cold" run served
                // Action Cache entries whose outputs the executors could not fetch.
                use_shared_store: LoreSharedStoreMode::Disabled,
                shared_store_path: LoreString::default(),
            },
            None,
        )
        .await;
        if status != 0 {
            bail!("lore repository create failed at {repo_path} (status={status})");
        }
        Ok(())
    }

    /// Read many entries. `Ok(None)` in a slot is a miss, not an error.
    ///
    /// `keys` are `(hash, size)` digest pairs. Result slots line up with the input order. A CAS
    /// read that misses the build cache is retried against the toolchain partition, so a seeded
    /// compiler is found without the caller knowing a toolchain exists.
    pub async fn get_many(&self, ns: Ns, keys: &[(String, i64)]) -> Result<Vec<Option<Vec<u8>>>> {
        self.get_many_as(ns, keys, Delivery::Content).await
    }

    /// [`get_many`](Self::get_many), each hit handed back as `delivery` says.
    pub async fn get_many_as(
        &self,
        ns: Ns,
        keys: &[(String, i64)],
        delivery: Delivery,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        // The empty blob has no key to resolve (see `put_many`), so it is produced rather than
        // read. Answering it here rather than in each caller is what stops a new one seeing a
        // spurious miss for a blob the REAPI says is always present.
        let empty = match delivery {
            Delivery::Content => Vec::new(),
            Delivery::Zstd => EMPTY_ZSTD_FRAME.to_vec(),
        };
        let mut out: Vec<Option<Vec<u8>>> = keys
            .iter()
            .map(|(hash, size)| is_empty_content(ns, hash, *size).then(|| empty.clone()))
            .collect();

        for part in Self::read_order(ns) {
            let pending: Vec<usize> = (0..keys.len()).filter(|i| out[*i].is_none()).collect();
            if pending.is_empty() {
                break;
            }
            let subset: Vec<(String, i64)> = pending.iter().map(|i| keys[*i].clone()).collect();
            for (i, found) in pending
                .iter()
                .zip(self.get_batch(*part, ns, &subset, delivery).await?)
            {
                if found.is_some() {
                    out[*i] = found;
                }
            }
        }

        // Content bytes either way, so the counters mean the same whatever the delivery.
        let hits = out.iter().filter(|slot| slot.is_some()).count() as u64;
        let bytes: u64 = out
            .iter()
            .zip(keys)
            .filter(|(slot, _)| slot.is_some())
            .map(|(_, (_, size))| (*size).max(0) as u64)
            .sum();
        self.record_get(ns, hits, out.len() as u64 - hits, bytes);
        Ok(out)
    }

    async fn get_batch(
        &self,
        part: Part,
        ns: Ns,
        keys: &[(String, i64)],
        delivery: Delivery,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();

        // Per-item accumulators, indexed by the `id` echoed on every event. `get_resolved` with
        // `streaming = 0` sends one DATA event per item, but honour `offset` anyway so a
        // fragmented reply reassembles correctly. Content is allocated at the size the digest
        // declares, and data arriving in order is appended rather than zero-filled and copied. A
        // zstd stream is built from one FRAGMENT event per leaf, in order, and its size is not
        // known.
        let sizes: Vec<usize> = keys
            .iter()
            .map(|(_, size)| match delivery {
                Delivery::Content => (*size).max(0) as usize,
                Delivery::Zstd => 0,
            })
            .collect();
        let bufs: Arc<Mutex<HashMap<u64, Vec<u8>>>> = Default::default();
        let codes: Arc<Mutex<HashMap<u64, Outcome>>> = Default::default();
        // Per item of a zstd read: how much content its leaves have covered, or why a leaf could
        // not be framed. Leaves arrive in content order, so each must start where the last ended.
        let framing: Arc<Mutex<HashMap<u64, std::result::Result<u64, String>>>> =
            Default::default();
        let (b, c, f) = (bufs.clone(), codes.clone(), framing.clone());
        let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| match ev {
            LoreEvent::StorageGetFragment(d) => {
                // The LoreBytes view is valid only during this callback; frame it now.
                let payload =
                    unsafe { std::slice::from_raw_parts(d.bytes.ptr.cast::<u8>(), d.bytes.len) };
                let mut framing = f.lock().unwrap();
                let state = framing.entry(d.id).or_insert(Ok(0));
                let next = match state {
                    Ok(covered) if d.offset == *covered => {
                        let mut guard = b.lock().unwrap();
                        zstd_frames::append_leaf(
                            guard.entry(d.id).or_default(),
                            &d.fragment,
                            payload,
                        )
                        .map(|()| *covered + d.fragment.size_content)
                    }
                    Ok(covered) => Err(format!(
                        "a leaf at content offset {} after {covered}",
                        d.offset
                    )),
                    Err(_) => return,
                };
                *state = next;
            }
            LoreEvent::StorageGetData(d) => {
                // The LoreBytes view is valid only during this callback; copy now.
                let slice =
                    unsafe { std::slice::from_raw_parts(d.bytes.ptr.cast::<u8>(), d.bytes.len) };
                let mut guard = b.lock().unwrap();
                let buf = guard.entry(d.id).or_insert_with(|| {
                    Vec::with_capacity(sizes.get(d.id as usize).copied().unwrap_or(0))
                });
                let offset = d.offset as usize;
                if offset == buf.len() {
                    buf.extend_from_slice(slice);
                } else {
                    let end = offset + slice.len();
                    if buf.len() < end {
                        buf.resize(end, 0);
                    }
                    buf[offset..end].copy_from_slice(slice);
                }
            }
            LoreEvent::StorageGetItemComplete(d) => {
                codes_insert(&c, d.id, &d.error);
            }
            _ => {}
        }));

        let items: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(i, (hash, size))| LoreStorageGetResolvedItem {
                id: i as u64,
                partition: self.partition(part),
                key: key(ns, hash, *size),
                context: self.context,
                streaming: 0,
                // Cache an upstream hit into the local tier, so the next read is local -- unless
                // the local tier is not meant to keep a copy; see `LocalCache`.
                local_cache: self.local_cache.blobs as u8,
                // Leaves as stored for zstd, which `zstd_frames` makes the stream of; see
                // `Delivery::Zstd`.
                fragments: (delivery == Delivery::Zstd) as u8,
                // No caller buffer: content arrives as `StorageGetData` events, or for zstd as
                // `StorageGetFragment` events, collected above.
                data_out: Default::default(),
            })
            .collect();

        // `status` is not the success signal -- a miss returns -1. Branch on the per-item
        // error_code, where AddressNotFound is the only reliable miss.
        let status = storage::get_resolved::get_resolved(
            globals(&self.correlation_id),
            LoreStorageGetResolvedArgs {
                handle: self.handle,
                items: LoreArray::from_vec(items),
            },
            cb,
        )
        .await;

        let codes = codes.lock().unwrap().clone();
        let framing = framing.lock().unwrap();
        let mut bufs = bufs.lock().unwrap();
        let mut out = Vec::with_capacity(keys.len());
        for i in 0..keys.len() {
            let id = i as u64;
            match codes.get(&id) {
                Some((OK, _)) => {
                    if let Some(Err(message)) = framing.get(&id) {
                        bail!(
                            "lore get_resolved for {ns:?} delivered a leaf that cannot be framed: {message}"
                        );
                    }
                    out.push(Some(bufs.remove(&id).unwrap_or_default()))
                }
                Some((NOT_FOUND, _)) | None => {
                    // A missing terminal event is treated as a miss rather than an error: the
                    // caller's recovery for both is identical, and reporting a spurious hard
                    // error would fail a build that could still make progress.
                    out.push(None)
                }
                Some((code, message)) => bail!(
                    "lore get_resolved failed for {ns:?}: {message} (code {code}, status={status})"
                ),
            }
        }

        // Counted by `get_many` over the final outcome, not here: a build-cache miss that the
        // toolchain partition then serves is one hit, and counting each phase would report it as
        // both a hit and a miss.
        if prof_enabled() {
            let hits = out.iter().filter(|slot| slot.is_some()).count();
            eprintln!(
                "LORE_PROF get ns={ns:?} part={part:?} items={} hits={hits} ms={:.1}",
                keys.len(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(out)
    }

    pub async fn get(&self, ns: Ns, hash: &str, size: i64) -> Result<Option<Vec<u8>>> {
        self.get_as(ns, hash, size, Delivery::Content).await
    }

    /// [`get`](Self::get), the hit handed back as `delivery` says.
    pub async fn get_as(
        &self,
        ns: Ns,
        hash: &str,
        size: i64,
        delivery: Delivery,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .get_many_as(ns, &[(hash.to_string(), size)], delivery)
            .await?
            .pop()
            .flatten())
    }

    /// Write many entries straight to files, never holding the content here.
    ///
    /// `get_many` writing to a path instead of to the callback. A miss leaves its destination
    /// untouched and reports `false` in that slot; slots line up with the input order.
    ///
    /// Each entry is `(hash, size, destination)`. Destination parents must already exist. The
    /// caller owns the mode: Lore restores content, not permissions.
    ///
    /// A miss in the build cache is retried against the toolchain partition, which is how a
    /// worker materialises a seeded compiler it never uploaded. A miss leaves its destination
    /// untouched, so the retry writes to a path the first attempt did not touch.
    pub async fn get_file_many(&self, entries: &[(String, i64, &Path)]) -> Result<Vec<bool>> {
        let mut out = self.get_file_phase(Part::BuildCache, entries).await?;

        let pending: Vec<usize> = (0..out.len()).filter(|i| !out[*i]).collect();
        if !pending.is_empty() {
            let subset: Vec<(String, i64, &Path)> =
                pending.iter().map(|i| entries[*i].clone()).collect();
            for (i, found) in pending
                .iter()
                .zip(self.get_file_phase(Part::Toolchain, &subset).await?)
            {
                out[*i] = found;
            }
        }

        let hits = out.iter().filter(|found| **found).count() as u64;
        let bytes: u64 = out
            .iter()
            .zip(entries)
            .filter(|(found, _)| **found)
            .map(|(_, (_, size, _))| (*size).max(0) as u64)
            .sum();
        self.record_get(Ns::Cas, hits, out.len() as u64 - hits, bytes);
        Ok(out)
    }

    async fn get_file_phase(
        &self,
        part: Part,
        entries: &[(String, i64, &Path)],
    ) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(entries.len());
        for chunk in entries.chunks(MAX_FILE_ITEMS_PER_CALL) {
            out.extend(self.get_file_batch(part, chunk).await?);
        }
        Ok(out)
    }

    async fn get_file_batch(
        &self,
        part: Part,
        entries: &[(String, i64, &Path)],
    ) -> Result<Vec<bool>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();

        let codes: Arc<Mutex<HashMap<u64, Outcome>>> = Default::default();
        let c = codes.clone();
        let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
            if let LoreEvent::StorageGetItemComplete(d) = ev {
                codes_insert(&c, d.id, &d.error);
            }
        }));

        // The empty blob is never stored (see `put_file_many`), so it is created here rather
        // than resolved.
        let mut out = vec![false; entries.len()];
        let mut items = Vec::with_capacity(entries.len());
        for (i, (hash, size, path)) in entries.iter().enumerate() {
            if digest::is_empty_blob(hash, *size) {
                tokio::fs::write(path, b"")
                    .await
                    .map_err(|e| anyhow!("writing empty blob to {}: {e}", path.display()))?;
                out[i] = true;
                continue;
            }
            items.push(LoreStorageGetFileResolvedItem {
                id: i as u64,
                partition: self.partition(part),
                key: key(Ns::Cas, hash, *size),
                context: self.context,
                path: path.to_string_lossy().as_ref().into(),
                offset: 0,
                length: 0,
                // Cache an upstream hit into the local tier, so the next action needing this
                // input reads it from disk instead of over the network -- unless the caller
                // keeps the file itself; see `LocalCache`.
                local_cache: self.local_cache.files as u8,
            });
        }
        if items.is_empty() {
            return Ok(out);
        }

        let status = storage::get_file_resolved::get_file_resolved(
            globals(&self.correlation_id),
            LoreStorageGetFileResolvedArgs {
                handle: self.handle,
                items: LoreArray::from_vec(items),
            },
            cb,
        )
        .await;

        let codes = codes.lock().unwrap();
        for (i, (_, _, path)) in entries.iter().enumerate() {
            if out[i] {
                continue;
            }
            match codes.get(&(i as u64)) {
                Some((OK, _)) => out[i] = true,
                Some((NOT_FOUND, _)) | None => {}
                Some((code, message)) => bail!(
                    "lore get_file_resolved failed for {}: {message} (code {code}, status={status})",
                    path.display()
                ),
            }
        }

        // Counted by `get_file_many` over the final outcome; see `get_batch`.
        if prof_enabled() {
            let hits = out.iter().filter(|found| **found).count();
            eprintln!(
                "LORE_PROF get_file part={part:?} items={} hits={hits} ms={:.1}",
                entries.len(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(out)
    }

    /// Store many entries in one Lore call. Each entry is `(hash, size, bytes)`.
    ///
    /// Zero-length content is skipped, for the reason given on `put_file_many`: it retracts a key
    /// rather than publishing one. In the CAS that is the empty blob, which the REAPI treats as
    /// always present. In the Action Cache it is decided by the content, not the key: an
    /// `ActionResult` that encodes to nothing is left unstored, which a lookup reads as a miss.
    pub async fn put_many(&self, ns: Ns, entries: &[(String, i64, &[u8])]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let started = Instant::now();

        let codes: Arc<Mutex<HashMap<u64, Outcome>>> = Default::default();
        let c = codes.clone();
        let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(d) = ev {
                codes_insert(&c, d.id, &d.error);
            }
        }));

        let items: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, (_, _, data))| !data.is_empty())
            .map(|(i, (hash, size, data))| LoreStoragePutResolvedItem {
                id: i as u64,
                partition: self.build_cache,
                key: key(ns, hash, *size),
                context: self.context,
                data: LoreBytes {
                    ptr: data.as_ptr().cast(),
                    len: data.len(),
                },
                // Publish upstream too when one is configured; the local store gets the content
                // and the mapping either way.
                remote_write: self.upstream as u8,
                // Whether to keep the payload locally, not just fragment metadata: the write
                // path retains it only when `!stored_durable || cache_local`, and a successful
                // upstream write makes it durable. Off, the local tier holds existence records
                // and reads go upstream; see `LocalCache`.
                local_cache: self.local_cache.blobs as u8,
                fixed_size_chunk: 0,
            })
            .collect();

        let expected: Vec<u64> = items.iter().map(|item| item.id).collect();
        if expected.is_empty() {
            return Ok(());
        }
        let status = storage::put_resolved::put_resolved(
            globals(&self.correlation_id),
            LoreStoragePutResolvedArgs {
                handle: self.handle,
                items: LoreArray::from_vec(items),
            },
            cb,
        )
        .await;

        let codes = codes.lock().unwrap();
        for i in expected {
            match codes.get(&i) {
                Some((OK, _)) => {}
                Some((code, message)) => bail!(
                    "lore put_resolved failed for {ns:?}: {message} (code {code}, status={status})"
                ),
                None => bail!(
                    "lore put_resolved reported no outcome for item {i} of {ns:?} (status={status})"
                ),
            }
        }

        let bytes: u64 = entries.iter().map(|(_, _, d)| d.len() as u64).sum();
        self.record_put(ns, entries.len() as u64, bytes);
        if prof_enabled() {
            eprintln!(
                "LORE_PROF put ns={ns:?} items={} bytes={bytes} ms={:.1}",
                entries.len(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(())
    }

    pub async fn put(&self, ns: Ns, hash: &str, size: i64, data: &[u8]) -> Result<()> {
        self.put_many(ns, &[(hash.to_string(), size, data)]).await
    }

    /// Store many entries in one Lore call, reading each from a file rather than from memory.
    ///
    /// `put_many` taking its content from a path. Each entry is `(hash, size, source)`; the
    /// caller has already hashed the file, and Lore reads it again itself, chunking a large one
    /// straight off disk.
    ///
    /// The empty blob is skipped. A zero-length file is a retraction to `put_file_resolved`, so
    /// storing one would delete whatever key it was published under, and the REAPI requires the
    /// empty blob to be treated as always present regardless.
    pub async fn put_file_many(&self, entries: &[(String, i64, &Path)]) -> Result<()> {
        self.put_file_into(Part::BuildCache, entries).await
    }

    /// Publish files into the durable partition.
    ///
    /// The seeding and indexing path, and the only write that does not go to the build cache. A
    /// build never takes it: what a build produces belongs to the build cache, where it can be
    /// evicted. Durable content is placed deliberately, once per toolchain version or pushed
    /// revision, and is expected to outlive every build that reads it.
    pub async fn seed_file_many(&self, entries: &[(String, i64, &Path)]) -> Result<()> {
        self.put_file_into(Part::Toolchain, entries).await
    }

    async fn put_file_into(&self, part: Part, entries: &[(String, i64, &Path)]) -> Result<()> {
        for chunk in entries.chunks(MAX_FILE_ITEMS_PER_CALL) {
            self.put_file_batch(part, chunk).await?;
        }
        Ok(())
    }

    async fn put_file_batch(&self, part: Part, entries: &[(String, i64, &Path)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let started = Instant::now();

        let codes: Arc<Mutex<HashMap<u64, Outcome>>> = Default::default();
        let c = codes.clone();
        let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(d) = ev {
                codes_insert(&c, d.id, &d.error);
            }
        }));

        let items: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, (hash, size, _))| !digest::is_empty_blob(hash, *size))
            .map(|(i, (hash, size, path))| LoreStoragePutFileResolvedItem {
                id: i as u64,
                partition: self.partition(part),
                key: key(Ns::Cas, hash, *size),
                context: self.context,
                path: path.to_string_lossy().as_ref().into(),
                remote_write: self.upstream as u8,
                local_cache: self.local_cache.files as u8,
                fixed_size_chunk: 0,
            })
            .collect();
        if items.is_empty() {
            return Ok(());
        }

        let expected: Vec<u64> = items.iter().map(|item| item.id).collect();
        let status = storage::put_file_resolved::put_file_resolved(
            globals(&self.correlation_id),
            LoreStoragePutFileResolvedArgs {
                handle: self.handle,
                items: LoreArray::from_vec(items),
            },
            cb,
        )
        .await;

        let codes = codes.lock().unwrap();
        for id in &expected {
            match codes.get(id) {
                Some((OK, _)) => {}
                Some((code, message)) => {
                    let path = entries[*id as usize].2.display();
                    bail!(
                        "lore put_file_resolved failed for {path}: {message} (code {code}, \
                         status={status})"
                    )
                }
                None => bail!(
                    "lore put_file_resolved reported no outcome for item {id} (status={status})"
                ),
            }
        }

        let bytes: u64 = expected
            .iter()
            .map(|id| entries[*id as usize].1.max(0) as u64)
            .sum();
        self.record_put(Ns::Cas, expected.len() as u64, bytes);
        if prof_enabled() {
            eprintln!(
                "LORE_PROF put_file items={} bytes={bytes} ms={:.1}",
                expected.len(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(())
    }

    /// Existence check for `FindMissingBlobs`, which must not pay for the content.
    ///
    /// `mutable_load` resolves the key -> content-hash mapping without reading the blob, but
    /// unlike `get_resolved` it does *not* tier: an item targets either the local or the remote
    /// mutable store. So this is local for everything, then one more call upstream for whatever
    /// missed -- two round trips worst case, regardless of how many digests were asked about.
    ///
    /// Resolving the key upstream is enough to answer "the server has it", even if the payload
    /// is not local yet: a later read tiers through and fetches it. The error direction that
    /// matters is the other one -- claiming presence for something unreadable would break the
    /// build, while a false absence only costs a re-upload.
    ///
    /// A CAS digest counts as present if *either* partition holds it, which is what stops bazel
    /// re-uploading a seeded toolchain. Both partitions are probed in the same call rather than
    /// in sequence, so the round-trip count is unchanged.
    pub async fn exists_many(&self, ns: Ns, keys: &[(String, i64)]) -> Result<Vec<bool>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let parts = Self::read_order(ns);
        let mut present = self.mutable_load_present(parts, ns, keys, false).await?;

        if self.upstream {
            let pending: Vec<(usize, (String, i64))> = keys
                .iter()
                .enumerate()
                .filter(|(i, _)| !present[*i])
                .map(|(i, k)| (i, k.clone()))
                .collect();
            if !pending.is_empty() {
                let subset: Vec<(String, i64)> = pending.iter().map(|(_, k)| k.clone()).collect();
                let upstream = self.mutable_load_present(parts, ns, &subset, true).await?;
                for ((idx, _), found) in pending.iter().zip(upstream) {
                    present[*idx] = found;
                }
            }
        }

        let found = present.iter().filter(|p| **p).count() as u64;
        self.stats
            .exists_present
            .fetch_add(found, Ordering::Relaxed);
        self.stats
            .exists_absent
            .fetch_add(present.len() as u64 - found, Ordering::Relaxed);
        Ok(present)
    }

    /// Whether each key resolves in any of `parts`, in one call.
    ///
    /// Every (partition, key) pair is its own item, with the partition encoded in the item id, so
    /// probing a second partition costs items rather than a round trip.
    async fn mutable_load_present(
        &self,
        parts: &[Part],
        ns: Ns,
        keys: &[(String, i64)],
        remote: bool,
    ) -> Result<Vec<bool>> {
        let codes: Arc<Mutex<HashMap<u64, Outcome>>> = Default::default();
        let c = codes.clone();
        let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
            if let LoreEvent::StorageMutableLoadItemComplete(d) = ev {
                codes_insert(&c, d.id, &d.error);
            }
        }));

        let items: Vec<_> = parts
            .iter()
            .enumerate()
            .flat_map(|(p, part)| {
                keys.iter()
                    .enumerate()
                    .map(move |(i, (hash, size))| LoreStorageMutableLoadItem {
                        id: (p * keys.len() + i) as u64,
                        partition: self.partition(*part),
                        key: key(ns, hash, *size),
                        key_type: KeyType::Resolve,
                    })
            })
            .collect();

        let g = if remote {
            remote_globals(&self.correlation_id)
        } else {
            globals(&self.correlation_id)
        };
        let status = storage::mutable_load::mutable_load(
            g,
            LoreStorageMutableLoadArgs {
                handle: self.handle,
                items: LoreArray::from_vec(items),
            },
            cb,
        )
        .await;

        let codes = codes.lock().unwrap();
        let count = keys.len();
        let mut out = vec![false; count];
        for (p, part) in parts.iter().enumerate() {
            for (i, present) in out.iter_mut().enumerate() {
                match codes.get(&((p * count + i) as u64)) {
                    Some((OK, _)) => *present = true,
                    Some((NOT_FOUND, _)) | None => {}
                    // Anything else (a dead upstream, say) is reported as absent rather than
                    // fatal: the caller re-uploads, which is always safe.
                    Some((code, message)) => tracing::debug!(
                        "mutable_load {ns:?} {part:?} remote={remote} item {i}: {message} \
                         (code {code}, status={status})"
                    ),
                }
            }
        }
        Ok(out)
    }

    fn record_get(&self, ns: Ns, hits: u64, misses: u64, bytes: u64) {
        match ns {
            Ns::Cas => {
                self.stats.cas_get_hit.fetch_add(hits, Ordering::Relaxed);
                self.stats.cas_get_miss.fetch_add(misses, Ordering::Relaxed);
                self.stats.cas_get_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Ns::Ac => {
                self.stats.ac_get_hit.fetch_add(hits, Ordering::Relaxed);
                self.stats.ac_get_miss.fetch_add(misses, Ordering::Relaxed);
            }
        }
    }

    fn record_put(&self, ns: Ns, items: u64, bytes: u64) {
        match ns {
            Ns::Cas => {
                self.stats.cas_put.fetch_add(items, Ordering::Relaxed);
                self.stats.cas_put_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Ns::Ac => {
                self.stats.ac_put.fetch_add(items, Ordering::Relaxed);
            }
        }
    }
}

/// How one item of a batched call ended: Lore's error code, and its message for reporting. The
/// message is copied out here because the event, and every string it points into, ends with the
/// callback.
type Outcome = (i32, String);

const OK: i32 = LoreErrorCode::None as i32;
/// The only reliable miss. Any other non-zero code is a failure, not an absence.
const NOT_FOUND: i32 = LoreErrorCode::AddressNotFound as i32;

fn codes_insert(map: &Arc<Mutex<HashMap<u64, Outcome>>>, id: u64, error: &LoreErrorDetail) {
    map.lock()
        .unwrap()
        .insert(id, (error.error_code, error.message.as_str().to_string()));
}

/// Make Lore's writes durable. Close's flush is otherwise fire-and-forget, so without this a
/// restart cannot reuse the on-disk repo -- which for a cold/warm benchmark would silently
/// throw away the warm cache. Run on a dedicated thread so it never observes an ambient tokio
/// runtime; it then flushes via Lore's shared runtime, sidestepping `block_in_place` ordering.
pub fn shutdown_lore() {
    let _ = std::thread::spawn(lore::shutdown).join();
}
