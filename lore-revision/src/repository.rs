// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Read;

use futures::StreamExt;
use lore_base::fs::lock::FSLock;
use lore_base::types::BranchPoint;
pub mod clone;
pub mod create;
pub mod delete;
pub mod dump;
pub mod info;
pub mod list;
pub mod status;
pub mod store;
pub mod verify;
mod write_token;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dashmap::DashMap;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use lore_base::lore_spawn;
use lore_base::lore_spawn_guarded;
use lore_error_set::prelude::*;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::RepositoryData;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use toml;
use zerocopy::IntoBytes;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter;
use crate::filter::Filter;
use crate::find;
use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::os::OsFilesystem;
use crate::global::GlobalConfig;
use crate::hash;
use crate::interface::LoreBranchLocation;
use crate::interface::LoreError;
use crate::interface::LoreGlobalArgs;
use crate::interface::LoreString;
use crate::layer;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_warn;
use crate::metadata::Metadata;
use crate::protocol;
use crate::revision;
use crate::revision::sync;
use crate::revision::sync::SyncOptions;
use crate::shared_store::get_shared_store_path_for_repo;
use crate::state;
use crate::store::ImmutableStore;
use crate::store::KeyType;
use crate::store::MutableStore;
use crate::store::immutable::ImmutableStoreCreateOptions;
use crate::store::immutable::ImmutableStoreSettings;
use crate::util;
use crate::util::path::PathError;
use crate::util::path::RelativePath;
use crate::util::path::make_absolute;

/// Details of the branch involved in a branch switch.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchSwitchData {
    /// Branch identifier.
    pub id: BranchId,
    /// Branch name.
    pub name: LoreString,
    /// Latest revision known locally for the branch.
    pub latest_local: Hash,
    /// Latest revision known on the remote for the branch.
    pub latest_remote: Hash,
    /// Revision the branch is switched to.
    pub revision: Hash,
    /// Where the branch exists: local, remote, or both.
    pub location: LoreBranchLocation,
}

impl LoreBranchSwitchData {
    fn new(
        id: BranchId,
        name: &str,
        latest_local: Hash,
        latest_remote: Hash,
        revision: Hash,
        location: LoreBranchLocation,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            latest_local,
            latest_remote,
            revision,
            location,
        }
    }
}

/// Data for the event emitted when a branch switch starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchSwitchBeginEventData {
    /// Details of the branch being switched to.
    pub branch: LoreBranchSwitchData,
}

/// Data for the event emitted when a branch switch finishes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchSwitchEndEventData {
    /// Details of the branch that was switched to.
    pub branch: LoreBranchSwitchData,
}

/// Data for the event emitted when a repository dump starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryDumpBeginEventData {
    /// Repository identifier.
    pub repository: RepositoryId,
    /// Revision being dumped.
    pub revision: Hash,
}

/// Data for the event emitted when a repository dump finishes.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryDumpEndEventData {
    /// Placeholder field. The event carries no data.
    pub _unused: u32,
}

/// Data for the event emitted when a repository configuration value is read.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryConfigGetEventData {
    /// Configuration key.
    pub key: LoreString,
    /// Configuration value for the key.
    pub value: LoreString,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct RepositoryConfig {
    pub remote_url: Option<String>,
    pub identity: Option<String>,
    #[serde(alias = "global_store_to_use")]
    pub shared_store_to_use: Option<SharedStoreToUseConfig>,
    pub store: Option<StoreConfig>,
    pub file: Option<FileConfig>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct StoreConfig {
    pub max_capacity: Option<usize>,
    pub eviction_delay: Option<usize>,
    pub max_size: Option<usize>,
    pub compaction_delay: Option<usize>,
    pub verify_write: Option<bool>,
}

impl StoreConfig {
    fn client_default() -> Self {
        StoreConfig {
            max_capacity: Some(2_000_000),
            eviction_delay: Some(30),
            max_size: Some(10 * 1024 * 1024 * 1024),
            compaction_delay: Some(50),
            verify_write: None,
        }
    }

    pub fn global_default() -> Self {
        StoreConfig {
            max_capacity: Some(2_000_000),
            eviction_delay: Some(30),
            max_size: Some(10 * 1024 * 1024 * 1024),
            compaction_delay: Some(50),
            verify_write: None,
        }
    }

    pub fn to_options(&self) -> ImmutableStoreCreateOptions {
        ImmutableStoreCreateOptions {
            max_capacity: Some(self.max_capacity.unwrap_or_default()),
            eviction_delay: Some(
                self.eviction_delay
                    .map(|sec| Duration::from_secs(sec as u64))
                    .unwrap_or_default(),
            ),
            max_size: Some(self.max_size.unwrap_or_default()),
            compaction_delay: Some(
                self.compaction_delay
                    .map(|sec| Duration::from_secs(sec as u64))
                    .unwrap_or_default(),
            ),
        }
    }
}

/// Decides whether the automatic incremental background GC (evictor + compactor) is
/// spawned for a freshly opened store, returning the caps to spawn it with or
/// [`ImmutableStoreCreateOptions::none`] to leave it unspawned.
///
/// Incremental GC is the default on write operations (`!read_only`). It is
/// suppressed by `--no-gc` and on dry-run commands (`suppress_incremental`, which
/// the caller folds together), since a dry run persists nothing and its background
/// tasks would otherwise race the dry run's own teardown. Read-only opens never
/// spawn it. When enabled but the repository has no `[store]` config, the built-in
/// [`StoreConfig::client_default`] caps apply.
fn incremental_gc_options(
    read_only: bool,
    suppress_incremental: bool,
    config_store: Option<&StoreConfig>,
) -> ImmutableStoreCreateOptions {
    if !read_only && !suppress_incremental {
        config_store
            .cloned()
            .unwrap_or_else(StoreConfig::client_default)
            .to_options()
    } else {
        ImmutableStoreCreateOptions::none()
    }
}

#[cfg(test)]
mod store_config_tests {
    use super::StoreConfig;
    use super::incremental_gc_options;

    /// The default GC caps that back the automatic incremental GC on writes when a
    /// repository has no `[store]` config must be populated and non-zero; otherwise the
    /// evictor and compactor would be spawned with zero caps (`Some(0)`) and never run.
    #[test]
    fn client_default_yields_nonzero_gc_caps() {
        let options = StoreConfig::client_default().to_options();
        assert!(options.max_capacity.is_some_and(|c| c > 0));
        assert!(options.max_size.is_some_and(|s| s > 0));
    }

    #[test]
    fn write_op_spawns_incremental_gc_by_default() {
        // Write op, `--no-gc` not set, no `[store]` config: incremental GC on with
        // the non-zero `client_default` caps.
        let options = incremental_gc_options(false, false, None);
        assert!(options.max_capacity.is_some_and(|c| c > 0));
        assert!(options.max_size.is_some_and(|s| s > 0));
    }

    #[test]
    fn no_gc_suppresses_incremental_gc() {
        let options = incremental_gc_options(false, true, None);
        assert!(options.max_capacity.is_none());
        assert!(options.max_size.is_none());
    }

    #[test]
    fn read_only_op_never_spawns_incremental_gc() {
        let options = incremental_gc_options(true, false, None);
        assert!(options.max_capacity.is_none());
        assert!(options.max_size.is_none());
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileConfig {
    direct_write: Option<bool>,
    direct_io: Option<bool>,
    flush_write: Option<bool>,
}

impl Default for FileConfig {
    fn default() -> Self {
        FileConfig {
            direct_write: Some(false),
            direct_io: Some(false),
            flush_write: Some(false),
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SharedStoreToUseConfig {
    #[serde(alias = "use_global_store")]
    pub use_shared_store: Option<bool>,
    #[serde(alias = "global_store_path")]
    pub shared_store_path: Option<String>,
}

impl SharedStoreToUseConfig {
    pub fn from_cli_args(
        global_config: &GlobalConfig,
        use_shared_store: u8,
        path: &LoreString,
    ) -> Result<Option<SharedStoreToUseConfig>, PathError> {
        if global_config
            .use_shared_store_automatically
            .unwrap_or(false)
            || use_shared_store != 0
        {
            Ok(Some(SharedStoreToUseConfig {
                use_shared_store: Some(true),
                shared_store_path: if let Some(path_string) = Into::<Option<&str>>::into(path) {
                    Some(make_absolute(path_string)?.to_string_lossy().to_string())
                } else {
                    None
                },
            }))
        } else {
            Ok(None)
        }
    }
}

pub struct RepositoryRuntimeSettings {
    /// Disable store fragments on remote
    pub disable_upload: AtomicBool,
    /// Disable caching fragments locally on get from remote (except state fragments which are always cached)
    pub disable_cache: AtomicBool,
    /// Write directly to target file instead of write to temporary file + move
    pub direct_file_write: AtomicBool,
    /// Use direct file I/O instead of memory mapping files
    pub direct_file_io: AtomicBool,
}

impl Default for RepositoryRuntimeSettings {
    fn default() -> Self {
        RepositoryRuntimeSettings {
            disable_upload: AtomicBool::new(true),
            disable_cache: AtomicBool::new(true),
            direct_file_write: AtomicBool::new(false),
            direct_file_io: AtomicBool::new(false),
        }
    }
}

impl Clone for RepositoryRuntimeSettings {
    fn clone(&self) -> Self {
        RepositoryRuntimeSettings {
            disable_upload: AtomicBool::new(self.disable_upload.load(Ordering::Relaxed)),
            disable_cache: AtomicBool::new(self.disable_cache.load(Ordering::Relaxed)),
            direct_file_write: AtomicBool::new(self.direct_file_write.load(Ordering::Relaxed)),
            direct_file_io: AtomicBool::new(self.direct_file_io.load(Ordering::Relaxed)),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.disable_upload.store(
            source.disable_upload.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.disable_cache.store(
            source.disable_cache.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.direct_file_write.store(
            source.direct_file_write.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.direct_file_io.store(
            source.direct_file_io.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}

/// Proof of write capability for a repository command.
///
/// Two shapes:
///
/// - [`Client`](Self::Client): carries a reference-counted guard on the
///   per-path write mutex (see [`write_token`]). The token's existence *is*
///   the mutex being held. You cannot mint one without acquiring the mutex,
///   and you cannot release the mutex while keeping the token. A command
///   that performs multiple context constructions in sequence (notably
///   clone: `create_local` → `load_and_connect` → filter rebuild) calls
///   [`share`](Self::share) to hand each construction a sibling token that
///   refcounts the same guard.
/// - [`Server`](Self::Server): carries nothing. The server's storage layer
///   serializes writes via per-bucket `RwLock`s, so the per-path write
///   mutex is not the concurrency boundary; the token exists purely as
///   type-level proof of write authorization for the handle API. Gated by
///   [`ServerContext`].
///
/// Tokens are stored as `Option<…>` on `RepositoryContext`; leaf write
/// sites fetch via `repository.try_write_token().ok_or(WriteRequired)?`.
#[must_use]
pub enum RepositoryWriteToken {
    Client(Arc<tokio::sync::OwnedMutexGuard<()>>),
    Server,
}

impl RepositoryWriteToken {
    /// Acquire the per-path write mutex and wrap the guard in a fresh
    /// client token. Blocks until any other in-process writer on this path
    /// releases.
    pub async fn acquire(path: &Path) -> Self {
        let mutex = write_token::write_mutex_for_path(path);
        let guard = mutex.lock_owned().await;
        Self::Client(Arc::new(guard))
    }

    /// Mint a server token. No mutex is taken — the server relies on the
    /// storage layer's per-bucket `RwLock`s for concurrency.
    ///
    /// Gated by [`ServerContext`]: only crates that explicitly opt in by
    /// implementing the marker trait for one of their own types can mint a
    /// server token. Within `lore-revision`, the internal marker
    /// [`InternalServerContext`] is used by the server-context constructors.
    pub fn server<S: ServerContext>(_: &S) -> Self {
        Self::Server
    }

    /// Produce a sibling token. For [`Client`](Self::Client), the sibling
    /// refcounts the same underlying mutex guard so multi-context commands
    /// keep the mutex held until every sibling drops. For
    /// [`Server`](Self::Server), returns another `Server` — nothing to
    /// share.
    ///
    /// Safe to expose: `share` does not circumvent the mutex (the guard is
    /// still held exactly once, just refcounted) and `acquire` is still the
    /// only way to mint a fresh client token. The intended uses are clone's
    /// multi-context setup and integration tests that mint a single token
    /// and hand siblings to the setup context(s) plus the final test
    /// context.
    pub fn share(&self) -> Self {
        match self {
            Self::Client(guard) => Self::Client(guard.clone()),
            Self::Server => Self::Server,
        }
    }
}

/// Marker trait that gates [`RepositoryWriteToken::server`].
///
/// Server-side crates implement this on a private unit type to opt in to
/// minting server tokens. The trait body is empty — it exists purely to
/// make "I am a server" a compile-time prerequisite for skipping the
/// per-path write mutex.
pub trait ServerContext {}

/// Internal marker used by `RepositoryContext::new_server_context` and
/// friends so this crate can mint server tokens without exposing a generic
/// constructor or a free function.
struct InternalServerContext;
impl ServerContext for InternalServerContext {}
const INTERNAL_SERVER_CONTEXT: InternalServerContext = InternalServerContext;

/// Shared, clone-able future that resolves the pending remote connection exactly once
/// while fanning the result out to all awaiters.
type RemoteFuture = Shared<BoxFuture<'static, Result<Arc<Connection>, ProtocolError>>>;

/// State machine for the remote connection. Lives behind `Arc<RwLock<_>>` so related
/// contexts (e.g. filter views) can share the same underlying connection state while
/// link/layer contexts build their own against a freshly-connected module.
pub(crate) enum RemoteState {
    /// No remote configured, or globals set offline — permanent terminal state.
    Offline,
    /// Connection attempt in flight; awaiters poll the shared future.
    Pending(RemoteFuture),
    /// Connection established.
    Connected(Arc<Connection>),
    /// Connection attempt failed; terminal state for this context.
    Failed(ProtocolError),
}

/// Public snapshot variant returned by `RepositoryContext::remote_status`. Mirrors
/// `RemoteState` without carrying the in-flight future, so callers never accidentally
/// drive the connect from a snapshot.
pub enum RemoteStatus {
    Offline,
    Pending,
    Connected(Arc<Connection>),
    Failed(ProtocolError),
}

pub struct RepositoryContext {
    /// Working-tree path for this repository. `None` for path-less contexts
    /// (server-side handlers, in-memory revision-tree handles) that operate
    /// only on the underlying stores. Code that walks the working tree calls
    /// [`RepositoryContext::require_path`] to fail with
    /// [`RepositoryError::InvalidArguments`] when the path is absent.
    pub path: Option<PathBuf>,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    file_system: Arc<dyn FilesystemProvider>,
    pub id: RepositoryId,
    pub instance_id: crate::instance::InstanceId,
    remote: Arc<tokio::sync::RwLock<RemoteState>>,
    pub filter: Arc<Filter>,
    pub format: RepositoryFormat,
    settings: RepositoryRuntimeSettings,
    is_link: bool,
    is_layer: bool,
    write_token: Option<RepositoryWriteToken>,
    repo_lock: Option<Arc<RepositoryLock>>,
}

impl std::fmt::Debug for RepositoryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RepositoryContext({})", self.id)
    }
}

impl RemoteState {
    /// Classify a resolved connect result into a terminal state. A `NoRemote` error
    /// reflects "no remote configured" rather than a failure, so it becomes `Offline`.
    fn from_result(remote: Result<Arc<Connection>, ProtocolError>) -> Self {
        match remote {
            Ok(conn) => RemoteState::Connected(conn),
            Err(ProtocolError::NoRemote(_)) => RemoteState::Offline,
            Err(err) => RemoteState::Failed(err),
        }
    }
}

fn remote_arc(state: RemoteState) -> Arc<tokio::sync::RwLock<RemoteState>> {
    Arc::new(tokio::sync::RwLock::new(state))
}

impl RepositoryContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: Option<PathBuf>,
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
        id: RepositoryId,
        instance_id: crate::instance::InstanceId,
        remote: Result<Arc<Connection>, ProtocolError>,
        filter: Arc<Filter>,
        format: RepositoryFormat,
    ) -> Self {
        Self::new_with_state(
            path,
            immutable_store,
            mutable_store,
            id,
            instance_id,
            RemoteState::from_result(remote),
            filter,
            format,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_state(
        path: Option<PathBuf>,
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
        id: RepositoryId,
        instance_id: crate::instance::InstanceId,
        remote: RemoteState,
        filter: Arc<Filter>,
        format: RepositoryFormat,
    ) -> Self {
        let file_system = Self::default_filesystem(path.as_deref().unwrap_or(Path::new("")));
        RepositoryContext {
            path,
            immutable_store,
            mutable_store,
            id,
            instance_id,
            remote: remote_arc(remote),
            filter,
            format,
            settings: RepositoryRuntimeSettings::default(),
            is_link: false,
            is_layer: false,
            write_token: None,
            repo_lock: None,
            file_system,
        }
    }

    /// Borrow the working-tree path required by filesystem-backed operations.
    /// Returns [`crate::errors::InvalidArguments`] for path-less contexts
    /// (server-side handlers, in-memory revision-tree handles); see the
    /// `path` field's documentation for the contract. Returning the FFI
    /// error directly lets `?` propagate into any caller `error_set` that
    /// carries an `InvalidArguments` variant.
    pub fn require_path(&self) -> Result<&Path, crate::errors::InvalidArguments> {
        self.path
            .as_deref()
            .ok_or_else(|| crate::errors::InvalidArguments {
                reason: "repository context has no working-tree path".to_string(),
            })
    }

    /// Display the working-tree path for logging and error messages. Renders
    /// `<unset>` for path-less contexts so log lines remain readable when the
    /// working tree is intentionally absent.
    pub fn path_for_display(&self) -> std::path::Display<'_> {
        self.path
            .as_deref()
            .unwrap_or_else(|| Path::new("<unset>"))
            .display()
    }

    pub fn salt(&self) -> &'static [u8] {
        self.format.salt()
    }

    /// Attach a process-local repository `FSLock` holder to this context. The
    /// lock is held for the lifetime of the context (and any clones that
    /// propagate the field). Used by `load_and_connect` on the disk-backed
    /// path; other construction paths leave `repo_lock` as `None`.
    #[must_use]
    pub(crate) fn with_repository_lock(mut self, lock: Arc<RepositoryLock>) -> Self {
        self.repo_lock = Some(lock);
        self
    }

    /// Attach a [`RepositoryWriteToken`] to this context. The token carries
    /// the per-path write-mutex guard, so the caller must have already
    /// obtained it via [`RepositoryWriteToken::acquire`] (fresh mint +
    /// guard) or `RepositoryWriteToken::share` (sibling of an already-held
    /// guard — crate-internal only).
    ///
    /// Normal write-mode contexts receive their token inside
    /// [`load_and_connect`] and [`create_local`]. This accessor is public
    /// primarily so integration tests can construct a write-capable context
    /// around their own fabricated stores; it is safe to expose because
    /// minting a token still requires acquiring the real per-path mutex.
    #[must_use]
    pub fn with_write_token(mut self, token: RepositoryWriteToken) -> Self {
        self.write_token = Some(token);
        self
    }

    /// Spawn a background task that flushes pending store writes to disk.
    /// Called by the client dispatcher after a command completes.
    ///
    /// The immutable store is flushed unconditionally: read-only commands
    /// can still dirty it by caching remote-fetched fragments locally (see
    /// `load_fragment` in `lore-storage`), so skipping the flush for
    /// read-only contexts would lose those cache writes on process exit.
    /// Immutable-store mutations are content-addressed and idempotent, so
    /// cross-process concurrent flushes are safe at the data-structure
    /// level.
    ///
    /// The mutable store is only flushed when the context holds a write
    /// token — read-only commands cannot have dirtied it.
    ///
    /// The repository `FSLock` is kept alive for the duration of the spawned
    /// flush by cloning the `Arc<RepositoryLock>` into the task. Without
    /// this, the lock would drop when the caller's `RepositoryContext`
    /// reference drops at end-of-command, allowing another process to
    /// enter and race this flush against its own writes to the per-bucket
    /// immutable-store index files.
    pub fn try_spawn_post_command_flush(&self, sync_data: bool) {
        let immutable_store = self.immutable_store.clone();
        let mutable_store = self
            .write_token
            .is_some()
            .then(|| self.mutable_store.clone());
        let repo_lock = self.repo_lock.clone();
        lore_base::runtime::LORE_CONTEXT.sync_scope(
            Arc::new(crate::interface::ExecutionContext::default())
                as Arc<dyn std::any::Any + Send + Sync>,
            || {
                lore_spawn_guarded!(async move {
                    // Let the store compaction run one step
                    immutable_store.clone().compact_stop().await;

                    let _ = immutable_store.flush(sync_data).await;
                    if let Some(mutable_store) = mutable_store {
                        let _ = mutable_store.flush(sync_data).await;
                    }
                    drop(repo_lock);
                });
            },
        );
    }

    /// Spawn a background task that holds strong references to the underlying
    /// stores for `duration`, keeping them upgradeable via the weak-ref cache
    /// so subsequent commands can reuse them. Called by the client dispatcher
    /// when `store_keep_alive` is configured. Not a write; safe to call
    /// regardless of the context's access mode.
    pub fn spawn_keep_alive(&self, duration: Duration) {
        let immutable_store = self.immutable_store.clone();
        let mutable_store = self.mutable_store.clone();
        lore_base::runtime::LORE_CONTEXT.sync_scope(
            Arc::new(crate::interface::ExecutionContext::default())
                as Arc<dyn std::any::Any + Send + Sync>,
            || {
                lore_spawn_guarded!(async move {
                    tokio::time::sleep(duration).await;
                    drop(immutable_store);
                    drop(mutable_store);
                });
            },
        );
    }

    pub fn immutable_store(&self) -> Arc<dyn ImmutableStore> {
        self.immutable_store.clone()
    }

    /// Read-only view of the mutable store. Always available; bound to the
    /// `&self` borrow so it cannot outlive the context reference.
    pub fn read_mutable_store<'a>(&'a self) -> crate::store::handles::ReadHandle<'a> {
        crate::store::handles::ReadHandle::new(&self.mutable_store)
    }

    /// Write-capable view of the mutable store. Requires a
    /// [`RepositoryWriteToken`] reference as compile-time proof of write
    /// authorization; the returned handle is bound to the token borrow's
    /// lifetime.
    pub fn write_mutable_store<'a>(
        &'a self,
        token: &'a RepositoryWriteToken,
    ) -> crate::store::handles::WriteHandle<'a> {
        crate::store::handles::WriteHandle::new(&self.mutable_store, token)
    }

    /// Opportunistic write access. Returns `Some(WriteHandle)` when the
    /// context was constructed for a write command, `None` otherwise. Used
    /// by code that performs skip-safe writes (e.g. cache updates deep in
    /// a read-mostly call chain) or by write-mode command callbacks that
    /// prefer the handle-based API over passing a token reference around.
    #[must_use]
    pub fn try_write_mutable_store(&self) -> Option<crate::store::handles::WriteHandle<'_>> {
        self.write_token
            .as_ref()
            .map(|token| crate::store::handles::WriteHandle::new(&self.mutable_store, token))
    }

    /// Owned `Arc` to the mutable store, gated on the write token. Returns
    /// `Some(Arc)` for write-mode contexts, `None` for read-only ones. Use
    /// when a fire-and-forget background task needs to perform writes and
    /// must outlive the borrow scope of a [`crate::store::handles::WriteHandle`].
    /// The Arc keeps the store alive for as long as the spawned task holds it.
    #[must_use]
    pub fn try_mutable_store_arc(&self) -> Option<Arc<dyn MutableStore>> {
        self.write_token
            .as_ref()
            .map(|_| self.mutable_store.clone())
    }

    /// Borrow the write capability token attached to this context, if any.
    /// Returns `Some(&token)` for write-mode contexts, `None` for
    /// read-only. Callers that need to fail with their own error type on
    /// read-only contexts pair this with `.ok_or(SomeError::WriteRequired)?`.
    #[must_use]
    pub fn try_write_token(&self) -> Option<&RepositoryWriteToken> {
        self.write_token.as_ref()
    }

    pub fn file_system(&self) -> Arc<dyn FilesystemProvider> {
        self.file_system.clone()
    }

    fn default_filesystem(path: &Path) -> Arc<dyn FilesystemProvider> {
        Arc::new(OsFilesystem::new(path))
    }

    /// Create a server-side repository context without filesystem provider.
    ///
    /// Server contexts don't need filesystem access as they operate on stores directly.
    pub fn new_server_context(
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
        id: RepositoryId,
    ) -> Self {
        RepositoryContext {
            file_system: Self::default_filesystem(Path::new("")),
            path: None,
            immutable_store,
            mutable_store,
            id,
            instance_id: crate::instance::InstanceId::default(),
            remote: remote_arc(RemoteState::Offline),
            filter: Arc::default(),
            format: RepositoryFormat::Lore,
            settings: RepositoryRuntimeSettings::default(),
            is_link: false,
            is_layer: false,
            write_token: Some(RepositoryWriteToken::server(&INTERNAL_SERVER_CONTEXT)),
            repo_lock: None,
        }
    }

    pub fn to_server_context(&self, id: RepositoryId) -> Self {
        RepositoryContext {
            path: self.path.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::Offline),
            filter: self.filter.clone(),
            format: self.format,
            settings: self.settings.clone(),
            is_link: false,
            is_layer: false,
            write_token: Some(RepositoryWriteToken::server(&INTERNAL_SERVER_CONTEXT)),
            repo_lock: None,
            file_system: self.file_system.clone(),
        }
    }

    pub fn new_null_context(
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> Self {
        RepositoryContext {
            file_system: Self::default_filesystem(Path::new("")),
            path: None,
            immutable_store,
            mutable_store,
            id: RepositoryId::default(),
            instance_id: crate::instance::InstanceId::default(),
            remote: remote_arc(RemoteState::Offline),
            filter: Arc::default(),
            format: RepositoryFormat::Lore,
            settings: RepositoryRuntimeSettings::default(),
            is_link: false,
            is_layer: false,
            write_token: Some(RepositoryWriteToken::server(&INTERNAL_SERVER_CONTEXT)),
            repo_lock: None,
        }
    }

    pub fn to_null_context(&self) -> Self {
        RepositoryContext {
            path: self.path.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id: RepositoryId::default(),
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::Offline),
            filter: self.filter.clone(),
            format: self.format,
            settings: self.settings.clone(),
            is_link: false,
            is_layer: false,
            write_token: None,
            repo_lock: None,
            file_system: self.file_system.clone(),
        }
    }

    /// Rebuild this context with a different filter view and remote connection,
    /// inheriting everything else — stores, ids, path, format, settings, repo
    /// lock, and write token. Used by clone (`repository/clone.rs`) to apply
    /// a filter view on top of an already-loaded context without re-exposing
    /// the raw `Arc<dyn MutableStore>`.
    ///
    /// The write token is propagated via [`RepositoryWriteToken::share`], so
    /// the rebuilt context's token is a sibling of `self`'s — the underlying
    /// per-path mutex guard stays held until every sibling drops.
    pub(crate) fn with_filter_and_remote(
        &self,
        filter: Arc<Filter>,
        remote: Result<Arc<Connection>, ProtocolError>,
    ) -> Self {
        RepositoryContext {
            path: self.path.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id: self.id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::from_result(remote)),
            filter,
            format: self.format,
            settings: self.settings.clone(),
            is_link: self.is_link,
            is_layer: self.is_layer,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            file_system: self.file_system.clone(),
        }
    }

    pub async fn to_link_context(&self, id: RepositoryId) -> Self {
        let remote = self.remote().await;
        let remote = if let Ok(remote) = remote {
            remote.connect_module(id).await
        } else {
            remote
        };
        let settings = self.settings.clone();
        RepositoryContext {
            path: self.path.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::from_result(remote)),
            filter: self.filter.clone(),
            format: self.format,
            settings,
            is_link: true,
            is_layer: false,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            file_system: self.file_system.clone(),
        }
    }

    pub async fn to_layer_context(&self, id: RepositoryId) -> Self {
        let remote = self.remote().await;
        let remote = if let Ok(remote) = remote {
            remote.connect_module(id).await
        } else {
            remote
        };
        let settings = self.settings.clone();
        RepositoryContext {
            path: self.path.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::from_result(remote)),
            filter: self.filter.clone(),
            format: self.format,
            settings,
            is_link: false,
            is_layer: true,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            file_system: self.file_system.clone(),
        }
    }

    pub fn to_filter_context(&self, filter: Arc<Filter>) -> Self {
        RepositoryContext {
            path: self.path.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id: self.id,
            instance_id: self.instance_id,
            remote: self.remote.clone(),
            filter,
            format: self.format,
            settings: self.settings.clone(),
            is_link: self.is_link,
            is_layer: self.is_layer,
            write_token: None,
            repo_lock: self.repo_lock.clone(),
            file_system: self.file_system.clone(),
        }
    }

    pub fn disable_cache(&self) -> bool {
        self.settings.disable_cache.load(Ordering::Relaxed)
    }

    pub fn set_disable_cache(&self, disable: bool) {
        self.settings
            .disable_cache
            .store(disable, Ordering::Relaxed);
    }

    pub fn disable_upload(&self) -> bool {
        self.settings.disable_upload.load(Ordering::Relaxed)
    }

    pub fn set_disable_upload(&self, disable: bool) {
        self.settings
            .disable_upload
            .store(disable, Ordering::Relaxed);
    }

    pub fn direct_file_io(&self) -> bool {
        self.settings.direct_file_io.load(Ordering::Relaxed)
    }

    pub fn set_direct_file_io(&self, direct: bool) {
        self.settings
            .direct_file_io
            .store(direct, Ordering::Relaxed);
    }

    pub fn direct_file_write(&self) -> bool {
        self.settings.direct_file_write.load(Ordering::Relaxed)
    }

    pub fn set_direct_file_write(&self, direct: bool) {
        self.settings
            .direct_file_write
            .store(direct, Ordering::Relaxed);
    }

    pub async fn flush(&self, sync_data: bool) -> Result<(), RepositoryError> {
        let immutable_store = self.immutable_store.clone();
        let immutable_result = immutable_store.flush(sync_data).await;

        let mutable_store = self.mutable_store.clone();
        let mutable_result = mutable_store.flush(sync_data).await;

        immutable_result
            .and(mutable_result)
            .forward::<RepositoryError>("Failed to flush stores to disk")
    }

    /// Returns the remote connection, awaiting the pending connect task on first call.
    /// Concurrent callers share a single in-flight connect via the state machine's
    /// `Shared` future. After resolution, the state is promoted so subsequent calls
    /// hit the read-lock fast path.
    pub async fn remote(&self) -> Result<Arc<Connection>, ProtocolError> {
        // Fast path: resolved states take only the read lock and return immediately.
        let fut = match &*self.remote.read().await {
            RemoteState::Offline => return Err(ProtocolError::from(NoRemote)),
            RemoteState::Connected(c) => return Ok(c.clone()),
            RemoteState::Failed(e) => return Err(e.clone()),
            RemoteState::Pending(fut) => fut.clone(),
        };

        // Slow path: await the shared future. All concurrent callers awaken together
        // when it resolves.
        let result = fut.await;

        // Opportunistically promote to the terminal state so future calls skip the
        // shared future entirely. Racing writers are harmless — the matches! guard
        // makes the second write a no-op.
        let mut guard = self.remote.write().await;
        if matches!(*guard, RemoteState::Pending(_)) {
            *guard = match &result {
                Ok(c) => RemoteState::Connected(c.clone()),
                Err(ProtocolError::NoRemote(_)) => RemoteState::Offline,
                Err(e) => RemoteState::Failed(e.clone()),
            };
        }
        result
    }

    /// Snapshot of the remote state. Awaits the state lock (cheap — only contended
    /// briefly during promotion) but never drives or awaits the pending connect.
    /// Callers that only care about the already-resolved outcome — e.g. teardown
    /// paths releasing sessions — use this to avoid forcing resolution of a connect
    /// that the command body never needed.
    pub async fn remote_status(&self) -> RemoteStatus {
        match &*self.remote.read().await {
            RemoteState::Offline => RemoteStatus::Offline,
            RemoteState::Pending(_) => RemoteStatus::Pending,
            RemoteState::Connected(c) => RemoteStatus::Connected(c.clone()),
            RemoteState::Failed(e) => RemoteStatus::Failed(e.clone()),
        }
    }

    pub fn is_link(&self) -> bool {
        self.is_link
    }

    pub fn is_layer(&self) -> bool {
        self.is_layer
    }
}

#[error_set]
pub enum RepositoryError {
    RepositoryAlreadyExists,
    RepositoryNotFound,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    BranchAlreadyExists,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    InvalidArguments,
    AddressNotFound,
    PayloadNotFound,
    AlreadyLinked,
    LayerNotFound,
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    SharedStoreNotFound,
    Conflict,
    NothingStaged,
    BranchAdvanced,
    LinkPathNotFound,
    NotALink,
    NotALayer,
    LockNotFound,
    LockNotOwned,
    TokenNotFound,
    IdenticalMetadata,
    LocalModifications,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    MaxHistorySearchDepth,
    NotConnected,
    MissingIdentity,
}

impl EventError for RepositoryError {
    fn translated(&self) -> LoreError {
        match self {
            RepositoryError::Disconnected(_) => LoreError::Connection,
            RepositoryError::SlowDown(_) => LoreError::SlowDown,
            RepositoryError::Oversized(_) => LoreError::Oversized,
            RepositoryError::FileNotFound(_) => LoreError::FileNotFound,
            RepositoryError::NotFound(_)
            | RepositoryError::RepositoryNotFound(_)
            | RepositoryError::BranchNotFound(_)
            | RepositoryError::RevisionNotFound(_)
            | RepositoryError::LayerNotFound(_)
            | RepositoryError::LinkNotFound(_)
            | RepositoryError::NodeNotFound(_) => LoreError::NotFound,
            RepositoryError::AddressNotFound(_) => LoreError::AddressNotFound,
            RepositoryError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            RepositoryError::InvalidPath(_) | RepositoryError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            RepositoryError::RepositoryAlreadyExists(_)
            | RepositoryError::BranchAlreadyExists(_) => LoreError::AlreadyExists,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepositoryAccess {
    NoStore,
    ReadOnly,
    ReadWrite,
}

pub const DOT_URC: &str = ".urc";
pub const DOT_LORE: &str = ".lore";
pub const ID: &str = "id";
pub const INSTANCE: &str = "instance";
pub const CONFIG: &str = "config.toml";
pub const SERVICE: &str = "service";
pub const DOT_URCIGNORE: &str = ".urcignore";
pub const DOT_LOREIGNORE: &str = ".loreignore";

pub const SALT_URC: &[u8] = b"urc";
// We cannot easily change this as it is also used on server to create
// the mutable keys - it would lose track of existing repos in production
// So for now keep using "urc" salt when hashing mutable keys
pub const SALT_LORE: &[u8] = b"urc";
//pub const SALT_LORE: &[u8] = b"lore";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryFormat {
    /// Legacy: .urc/, .urcignore, salt b"urc"
    Urc,
    /// Current: .lore/, .loreignore, salt b"urc"
    Lore,
}

impl RepositoryFormat {
    pub fn salt(&self) -> &'static [u8] {
        match self {
            Self::Urc => SALT_URC,
            Self::Lore => SALT_LORE,
        }
    }

    pub fn dot_dir(&self) -> &'static str {
        match self {
            Self::Urc => DOT_URC,
            Self::Lore => DOT_LORE,
        }
    }

    /// Primary ignore file. Both formats use `.loreignore`; legacy
    /// `.urcignore` is honored only as a fallback (see [`load_filter`]).
    pub fn ignore_file(&self) -> &'static str {
        DOT_LOREIGNORE
    }

    pub fn detect(path: &std::path::Path) -> Self {
        if path.join(DOT_URC).is_dir() {
            Self::Urc
        } else {
            Self::Lore
        }
    }
}
pub const VIEW_FILTER: &str = "view";
pub const LAYER: &str = "layer.toml";
pub const TEMP_FILE_EXTENSION: &str = ".~loretemp";
pub const BASE_SUFFIX: &str = "~base";
pub const THEIRS_SUFFIX: &str = "~theirs";
pub const MINE_SUFFIX: &str = "~mine";

pub fn parse_url(url: &str, offline: bool) -> Result<(String, String), RepositoryError> {
    let url = if url.contains("://") {
        url::Url::parse(url).internal("Invalid URL")?
    } else {
        // Offline support for just a name
        if offline && !url.contains('/') {
            return Ok((String::default(), url.to_string()));
        }
        let mut protocol_url = lore_transport::DEFAULT_PROTOCOL.to_string();
        protocol_url.push_str("://");
        protocol_url.push_str(url);
        url::Url::parse(protocol_url.as_str()).internal("Invalid URL")?
    };
    let protocol = url.scheme();
    let host = url
        .host_str()
        .ok_or_else(|| RepositoryError::internal("Invalid URL"))?;
    let name = url.path().trim_matches('/');
    if name.is_empty() {
        return Err(RepositoryError::internal("Invalid URL"));
    }

    let mut remote_url = protocol.to_string();
    remote_url.push_str("://");
    remote_url.push_str(host);
    if let Some(port) = url.port() {
        remote_url.push_str(format!(":{port}").as_str());
    }

    Ok((remote_url, name.to_string()))
}

fn load_config(config_path: impl AsRef<Path>) -> Result<RepositoryConfig, RepositoryError> {
    // Synchronous read: tiny config file, avoids thread hop and queuing behind
    // any store flush tasks still in flight from the previous command.
    let config = match std::fs::read_to_string(config_path) {
        Ok(config) => config,
        Err(_) => return Ok(RepositoryConfig::default()),
    };
    Ok(toml::from_str(config.as_str()).internal("Failed to load config file")?)
}

async fn save_config(
    config_path: impl AsRef<Path>,
    config: &RepositoryConfig,
) -> Result<(), RepositoryError> {
    let mut config_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(config_path)
        .await
        .internal("Failed to save config file")?;

    let config_string = toml::to_string_pretty(&config).internal("Failed to save config file")?;

    config_file
        .write_all(config_string.as_bytes())
        .await
        .internal("Failed to save config file")?;
    config_file
        .flush()
        .await
        .internal("Failed to save config file")?;
    Ok(())
}

pub fn load_repository_config(path: impl AsRef<Path>) -> Result<RepositoryConfig, RepositoryError> {
    let path = path.as_ref();
    let dot_path = path.join(RepositoryFormat::detect(path).dot_dir());
    let config_path = dot_path.join(CONFIG);

    load_config(config_path.as_path())
}

/// Process-local holder for the repository directory `FSLock`. Lifetime is
/// managed via an `Arc` + weak-ref cache so overlapping commands share one
/// acquisition; the underlying flock drops when the last strong reference is
/// released.
pub(crate) struct RepositoryLock {
    _lock: FSLock,
}

static REPOSITORY_LOCK_CACHE: OnceLock<DashMap<PathBuf, Weak<RepositoryLock>>> = OnceLock::new();

/// Per-path init mutexes used to serialize first-time flock acquisition for a
/// given repository path. Held only during the double-checked-locking init
/// dance in `get_or_create_repository_lock`. The sync `RwLock` around the map
/// is held only briefly to read or insert a per-path `tokio::sync::Mutex`; it
/// is never held across an `.await`.
static REPOSITORY_LOCK_INIT_MUTEXES: OnceLock<
    std::sync::RwLock<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();

/// Acquire (or reuse, via weak-ref cache) the process-local `RepositoryLock`
/// for a repository's dot-directory (`.lore/` or `.urc/`).
///
/// Overlapping commands in the same process share one `Arc<RepositoryLock>` —
/// the OS-level flock is acquired once and released when the last strong
/// reference drops. Concurrent first-time initializers are serialized via a
/// per-path `tokio::sync::Mutex` so only one thread calls into the OS flock
/// for a given path at a time; others wait, then observe the cached entry.
pub(crate) async fn get_or_create_repository_lock(
    dot_path: PathBuf,
) -> Result<Arc<RepositoryLock>, RepositoryError> {
    let cache = REPOSITORY_LOCK_CACHE.get_or_init(DashMap::new);

    // Fast path: upgrade an existing weak reference.
    if let Some(holder) = cache.get(&dot_path).and_then(|w| w.upgrade()) {
        return Ok(holder);
    }

    // Slow path: serialize init attempts per path so only one of them calls
    // into the OS flock.
    let mutexes =
        REPOSITORY_LOCK_INIT_MUTEXES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    let init_mutex = {
        if let Some(existing) = mutexes.read().unwrap().get(&dot_path) {
            existing.clone()
        } else {
            let mut w = mutexes.write().unwrap();
            w.entry(dot_path.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        }
    };

    let _guard = init_mutex.lock().await;

    // Re-check the cache — another task may have finished initializing while
    // we were waiting on the init mutex.
    if let Some(holder) = cache.get(&dot_path).and_then(|w| w.upgrade()) {
        return Ok(holder);
    }

    // Acquire the OS flock on the blocking pool so a slow flock doesn't stall
    // the runtime.
    let path_for_lock = dot_path.clone();
    let lock = lore_base::runtime::runtime()
        .spawn_blocking(move || FSLock::acquire_directory_lock(path_for_lock))
        .await
        .internal("Failed to get exclusive access to repository")?
        .internal("Failed to get exclusive access to repository")?;

    let holder = Arc::new(RepositoryLock { _lock: lock });
    cache.insert(dot_path, Arc::downgrade(&holder));
    Ok(holder)
}

static IMMUTABLE_STORE_CACHE: OnceLock<DashMap<PathBuf, Weak<dyn ImmutableStore>>> =
    OnceLock::new();

static MUTABLE_STORE_CACHE: OnceLock<DashMap<PathBuf, Weak<crate::store::mutable::MutableStore>>> =
    OnceLock::new();

static IN_MEMORY_IMMUTABLE_CACHE: OnceLock<DashMap<PathBuf, Arc<dyn ImmutableStore>>> =
    OnceLock::new();

static IN_MEMORY_MUTABLE_CACHE: OnceLock<DashMap<PathBuf, Arc<dyn MutableStore>>> = OnceLock::new();

fn get_cached_immutable_store(path: &PathBuf) -> Option<Arc<dyn ImmutableStore>> {
    IMMUTABLE_STORE_CACHE
        .get_or_init(DashMap::new)
        .get(path)
        .and_then(|store| store.upgrade())
}

fn cache_immutable_store(path: PathBuf, store: Arc<dyn ImmutableStore>) {
    if let Some(map) = IMMUTABLE_STORE_CACHE.get() {
        map.insert(path, Arc::downgrade(&store));
    }
}

fn get_cached_mutable_store(path: &PathBuf) -> Option<Arc<crate::store::mutable::MutableStore>> {
    MUTABLE_STORE_CACHE
        .get_or_init(DashMap::new)
        .get(path)
        .and_then(|store| store.upgrade())
}

fn cache_mutable_store(path: PathBuf, store: Arc<crate::store::mutable::MutableStore>) {
    if let Some(map) = MUTABLE_STORE_CACHE.get() {
        map.insert(path, Arc::downgrade(&store));
    }
}

/// Per-path init mutexes for the mutable-store cache. Serialize concurrent
/// first-time construction of a store for the same path so only one caller
/// reaches into `LocalMutableStore::new` (which acquires the on-disk flock).
/// Held only during the double-checked-locking dance in
/// `create_client_mutable_store`; the sync `RwLock` around the map is never
/// held across an `.await`.
static MUTABLE_STORE_INIT_MUTEXES: OnceLock<
    std::sync::RwLock<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();

/// Per-path init mutexes for the immutable-store cache. Same purpose as
/// `MUTABLE_STORE_INIT_MUTEXES` but for the immutable store construction path.
static IMMUTABLE_STORE_INIT_MUTEXES: OnceLock<
    std::sync::RwLock<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();

pub fn get_cached_in_memory_stores(
    path: &PathBuf,
) -> Option<(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>)> {
    let imm = IN_MEMORY_IMMUTABLE_CACHE
        .get_or_init(DashMap::new)
        .get(path)
        .map(|r| r.clone())?;
    let mut_ = IN_MEMORY_MUTABLE_CACHE
        .get_or_init(DashMap::new)
        .get(path)
        .map(|r| r.clone())?;
    Some((imm, mut_))
}

pub fn cache_in_memory_stores(
    path: PathBuf,
    immutable: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
) {
    IN_MEMORY_IMMUTABLE_CACHE
        .get_or_init(DashMap::new)
        .insert(path.clone(), immutable);
    IN_MEMORY_MUTABLE_CACHE
        .get_or_init(DashMap::new)
        .insert(path, mutable);
}

/// Release all cached store references for the given repository path.
/// Any active `RepositoryContext` instances for this path remain valid
/// (they hold their own `Arc` to the stores), but once they are dropped
/// the stores will be freed. Subsequent opens will create fresh stores.
pub fn repository_release(path: impl AsRef<Path>) {
    let path = path.as_ref();
    for dot_dir in [DOT_URC, DOT_LORE] {
        let dot_path = path.join(dot_dir);
        if let Some(cache) = IMMUTABLE_STORE_CACHE.get() {
            cache.remove(&dot_path);
        }
        if let Some(cache) = MUTABLE_STORE_CACHE.get() {
            cache.remove(&dot_path);
        }
        if let Some(cache) = IN_MEMORY_IMMUTABLE_CACHE.get() {
            cache.remove(&dot_path);
        }
        if let Some(cache) = IN_MEMORY_MUTABLE_CACHE.get() {
            cache.remove(&dot_path);
        }
    }
}

/// Forwards [`lore_storage::gc_event::GcEventSink`] callbacks to a captured
/// execution context's dispatcher as the public eviction/compaction event
/// series. Carrying the context explicitly (rather than reading task-local
/// state) keeps routing correct when GC passes for different stores run
/// concurrently in one process.
struct GcEventForwarder {
    context: Arc<crate::interface::ExecutionContext>,
}

impl lore_storage::gc_event::GcEventSink for GcEventForwarder {
    fn eviction_begin(&self, target_fragments: u64) {
        self.context
            .dispatcher
            .send(event::LoreEvent::EvictionBegin(
                event::LoreEvictionBeginEventData { target_fragments },
            ));
    }

    fn eviction_progress(&self, evicted: u64) {
        self.context
            .dispatcher
            .send(event::LoreEvent::EvictionProgress(
                event::LoreEvictionProgressEventData { evicted },
            ));
    }

    fn eviction_end(&self, total_evicted: u64) {
        self.context.dispatcher.send(event::LoreEvent::EvictionEnd(
            event::LoreEvictionEndEventData { total_evicted },
        ));
    }

    fn compaction_begin(&self, target_bytes: u64) {
        self.context
            .dispatcher
            .send(event::LoreEvent::CompactionBegin(
                event::LoreCompactionBeginEventData { target_bytes },
            ));
    }

    fn compaction_progress(&self, compacted_bytes: u64) {
        self.context
            .dispatcher
            .send(event::LoreEvent::CompactionProgress(
                event::LoreCompactionProgressEventData { compacted_bytes },
            ));
    }

    fn compaction_end(&self, total_compacted_bytes: u64) {
        self.context
            .dispatcher
            .send(event::LoreEvent::CompactionEnd(
                event::LoreCompactionEndEventData {
                    total_compacted_bytes,
                },
            ));
    }
}

/// Builds a GC event sink bound to the current execution context, if one is
/// active. Returns `None` outside a command, where there is no callback to
/// forward to.
pub(crate) fn gc_event_sink() -> Option<lore_storage::gc_event::GcEventSinkRef> {
    crate::runtime::try_execution_context().map(|context| {
        Arc::new(GcEventForwarder { context }) as lore_storage::gc_event::GcEventSinkRef
    })
}

pub async fn create_client_immutable_store(
    config: &RepositoryConfig,
    dotpath: impl AsRef<Path>,
    create_options: ImmutableStoreCreateOptions,
    verify_write: bool,
) -> Result<Arc<dyn ImmutableStore>, RepositoryError> {
    let path = get_shared_store_path_for_repo(config)
        .await
        .forward::<RepositoryError>("Failed to access shared store")?
        .unwrap_or_else(|| dotpath.as_ref().to_owned());

    // Fast path: upgrade an existing weak reference.
    if let Some(store) = get_cached_immutable_store(&path) {
        lore_debug!("Reusing cached immutable store");
        return Ok(store);
    }

    // Slow path: serialize concurrent first-time initializers per path so only
    // one caller reaches `LocalImmutableStore::new` (which acquires the on-disk
    // flock). Without this, two overlapping cache misses would both block on
    // the OS flock for the same path.
    let mutexes =
        IMMUTABLE_STORE_INIT_MUTEXES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    let init_mutex = {
        if let Some(existing) = mutexes.read().unwrap().get(&path) {
            existing.clone()
        } else {
            let mut w = mutexes.write().unwrap();
            w.entry(path.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        }
    };

    let _guard = init_mutex.lock().await;

    // Re-check the cache — another task may have finished while we waited.
    if let Some(store) = get_cached_immutable_store(&path) {
        lore_debug!("Reusing cached immutable store");
        return Ok(store);
    }

    let store = crate::store::immutable::create(
        Some(path.as_path()),
        create_options,
        false, /* Don't deserialize all buckets on load */
        ImmutableStoreSettings {
            allow_partial_fragment: true, /* Client store can have partial fragments */
            protect_local_fragment: true, /* Protect local fragments from eviction */
            verify_write,
            ..Default::default()
        },
    )
    .await
    .forward::<RepositoryError>("Failed to create local store")?;

    // Let storage-layer load-triggered GC passes obtain a sink bound to the calling
    // command's context (idempotent; first registration wins).
    lore_storage::gc_event::set_gc_event_sink_provider(gc_event_sink);
    lore_storage::maintenance::spawn_gc(&store, &create_options);

    cache_immutable_store(path, store.clone());

    Ok(store)
}

pub async fn create_client_mutable_store(
    config: &RepositoryConfig,
    dotpath: impl AsRef<Path>,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<Arc<crate::store::mutable::MutableStore>, RepositoryError> {
    let dotpath = dotpath.as_ref().to_owned();

    let path = if let Some(global_path) =
        get_shared_store_path_for_repo(config)
            .await
            .forward::<RepositoryError>("Failed to access shared store")?
    {
        // Reject repositories that have a local mutable store alongside a
        // shared store configuration. Automatic migration is not supported.
        let local_mutable = dotpath.join("mutable");
        if local_mutable.exists() {
            return Err(RepositoryError::internal(
                "This repository has a local mutable store but is configured to use a shared store. Reclone the repository with `lore clone --use-shared-store` to use the shared mutable store.",
            ));
        }

        global_path
    } else {
        dotpath
    };

    // Fast path: upgrade an existing weak reference.
    if let Some(store) = get_cached_mutable_store(&path) {
        lore_debug!("Reusing cached mutable store");
        return Ok(store);
    }

    // Slow path: serialize concurrent first-time initializers per path so only
    // one caller reaches `LocalMutableStore::new` (which acquires the on-disk
    // flock). Without this, two overlapping cache misses would both block on
    // the OS flock for the same path.
    let mutexes = MUTABLE_STORE_INIT_MUTEXES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    let init_mutex = {
        if let Some(existing) = mutexes.read().unwrap().get(&path) {
            existing.clone()
        } else {
            let mut w = mutexes.write().unwrap();
            w.entry(path.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        }
    };

    let _guard = init_mutex.lock().await;

    // Re-check the cache — another task may have finished while we waited.
    if let Some(store) = get_cached_mutable_store(&path) {
        lore_debug!("Reusing cached mutable store");
        return Ok(store);
    }

    let store = crate::store::mutable::create(
        Some(path.as_path()),
        crate::store::mutable::MutableStoreSettings::default(),
        immutable_store,
    )
    .await
    .forward::<RepositoryError>("Failed to create local store")?;

    cache_mutable_store(path, store.clone());

    Ok(store)
}

pub async fn create_client_memory_stores()
-> Result<(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>), RepositoryError> {
    let immutable = crate::store::immutable::create(
        None::<&Path>,
        ImmutableStoreCreateOptions::none(),
        false, /* Client does not deserialize all buckets on startup */
        ImmutableStoreSettings {
            allow_partial_fragment: true, /* Client store can have partial fragments */
            protect_local_fragment: true, /* Protect local fragments from eviction */
            ..Default::default()
        },
    )
    .await
    .forward::<RepositoryError>("Failed to create local store")?;
    let mutable: Arc<dyn MutableStore> = crate::store::mutable::create(
        None::<&Path>,
        crate::store::mutable::MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .forward::<RepositoryError>("Failed to create local store")?;
    Ok((immutable, mutable))
}

fn connect(
    repository: RepositoryId,
    globals: &LoreGlobalArgs,
    config: &RepositoryConfig,
    identity: Option<String>,
) -> Result<RemoteFuture, ProtocolError> {
    if globals.offline() {
        return Err(ProtocolError::from(NoRemote));
    }

    let remote_url = config.remote_url.clone();
    remote_url.as_ref().inspect(|url| {
        lore_debug!("Using repository config remote: {url}",);
    });
    if remote_url.as_deref().is_none_or(|url| url.is_empty()) {
        return Err(ProtocolError::from(NoRemote));
    }

    let remote_url = remote_url.as_deref().unwrap_or_default().to_string();
    let identity = identity.as_deref().unwrap_or_default().to_string();

    let correlation_id = globals.correlation_id.to_string();
    let handle: JoinHandle<Result<Arc<Connection>, ProtocolError>> = lore_spawn!(async move {
        let connection =
            protocol::connect(remote_url.as_str(), identity.as_str(), repository).await?;
        // Pre-warm session so it's ready when the command runs
        if !repository.is_zero() {
            let _ = connection.session(repository, &correlation_id).await;
        }
        Ok(connection)
    });

    // Wrap the JoinHandle in a Shared future so many awaiters can poll one spawned task.
    Ok(async move {
        handle
            .await
            .unwrap_or_else(|_| Err(ProtocolError::internal("connect task failed")))
    }
    .boxed()
    .shared())
}

fn read_id_from_file(path: PathBuf) -> io::Result<RepositoryId> {
    let mut id = RepositoryId::default();
    // Synchronous read: tiny file, avoids thread hop and queuing behind
    // any store flush tasks still in flight from the previous command.
    std::fs::File::open(path)?.read_exact(id.as_mut_bytes())?;
    Ok(id)
}

/// Open an existing on-disk repository and return a ready-to-use context.
///
/// For `RepositoryAccess::ReadWrite`, acquires the per-path write mutex (via
/// [`RepositoryWriteToken::acquire`]) before the rest of setup so any other
/// in-process writer on the same path blocks until this command completes.
/// Callers that already hold a token (notably the clone flow, which builds
/// multiple contexts in sequence) should use [`load_and_connect_with_token`]
/// instead and pass a shared sibling to avoid re-acquiring the mutex.
pub async fn load_and_connect(
    path: impl AsRef<Path>,
    access: RepositoryAccess,
) -> Result<Arc<RepositoryContext>, RepositoryError> {
    let path = path.as_ref();
    let write_token = if access == RepositoryAccess::ReadWrite {
        Some(RepositoryWriteToken::acquire(path).await)
    } else {
        None
    };
    load_and_connect_with_token(path, access, write_token).await
}

/// Variant of [`load_and_connect`] that takes a pre-acquired write token.
///
/// The `(access, write_token)` pair must match one of:
///
/// - `(ReadOnly, None)` — read-only command, no writes possible.
/// - `(ReadWrite, Some(Client(_)))` — write command, per-path mutex held.
/// - `(NoStore, None)` — read-only on private in-memory stores.
/// - `(NoStore, Some(Client(_)))` — write command on private in-memory
///   stores (e.g. `clone --no-tracking`). The Client mutex is held by the
///   caller for cross-thread exclusion on the destination path; the stores
///   themselves are in-memory.
///
/// Used by commands that construct several contexts in sequence on the same
/// path (currently only clone); they mint one token at the top of the
/// command and hand siblings (via [`RepositoryWriteToken::share`]) to each
/// construction, keeping the per-path write mutex held across the whole
/// flow without deadlocking on re-acquisition.
pub async fn load_and_connect_with_token(
    path: &Path,
    access: RepositoryAccess,
    write_token: Option<RepositoryWriteToken>,
) -> Result<Arc<RepositoryContext>, RepositoryError> {
    debug_assert!(
        matches!(
            (&access, &write_token),
            (RepositoryAccess::ReadOnly, None)
                | (
                    RepositoryAccess::ReadWrite,
                    Some(RepositoryWriteToken::Client(_))
                )
                | (
                    RepositoryAccess::NoStore,
                    None | Some(RepositoryWriteToken::Client(_))
                )
        ),
        "write_token kind must match access mode (got access={access:?}, token kind={})",
        match &write_token {
            None => "None",
            Some(RepositoryWriteToken::Client(_)) => "Some(Client)",
            Some(RepositoryWriteToken::Server) => "Some(Server)",
        },
    );

    let format = RepositoryFormat::detect(path);
    let dot_path = path.join(format.dot_dir());

    // Acquire (or reuse) the process-local repository flock. NoStore commands
    // skip this — they don't touch repository files.
    let repo_lock = if access != RepositoryAccess::NoStore {
        Some(get_or_create_repository_lock(dot_path.clone()).await?)
    } else {
        None
    };

    let id_path = dot_path.join(ID);
    let config_path = dot_path.join(CONFIG);

    let repository = read_id_from_file(id_path).internal("Repository not found")?;

    let instance_path = dot_path.join(INSTANCE);
    let instance_id = match crate::instance::InstanceId::read_from_file(instance_path.clone()) {
        Ok(id) => id,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if access == RepositoryAccess::NoStore {
                return Err(RepositoryError::internal(
                    "Repository requires migration, open with disk access first",
                ));
            }
            // Instance ID will be recovered or generated after stores are
            // available. Use a placeholder for now.
            crate::instance::InstanceId::default()
        }
        Err(err) => {
            return Err(err).internal("Repository not found")?;
        }
    };
    let context = execution_context();
    let global = context.globals();
    let mut config = load_config(config_path.as_path())?;

    let mut identity = config.identity.clone();
    let mut identity_resolved_from_auth = false;
    if let Some(given_identity) = global.identity() {
        lore_debug!("Using given identity: {}", given_identity);
        identity = Some(given_identity.to_string());
    } else if let Some(config_identity) = identity.clone() {
        lore_debug!("Using repository config identity: {config_identity}");
    } else if !global.offline()
        && let Some(remote_url) = config.remote_url.as_deref()
        && !remote_url.is_empty()
    {
        // Establish a connection so the transport's `auth_exchange` picks
        // an identity scoped to this repo's auth server (rather than the
        // first cached identity across all servers). The transport caches
        // the connection, so the lazy `connect()` below reuses it rather
        // than reconnecting. Skipped when --offline: the connection would
        // fail and picking blindly from the cache risks the wrong
        // identity.
        if let Ok(conn) = protocol::connect(remote_url, "", repository).await {
            let resolved = conn.identity();
            if !resolved.is_empty() {
                lore_debug!("Using cached auth identity: {resolved}");
                identity = Some(resolved.to_string());
                identity_resolved_from_auth = true;
            }
        }
    }

    if identity_resolved_from_auth
        && matches!(access, RepositoryAccess::ReadWrite)
        && let Some(ref auth_identity) = identity
    {
        config.identity = Some(auth_identity.clone());
        let _ = save_config(config_path.as_path(), &config).await;
    }

    let execution = execution_context();
    if execution.user_id().await.is_empty() && identity.is_some() {
        execution
            .set_user_id(identity.as_deref().unwrap_or_default())
            .await;
    }

    let remote = connect(repository, global, &config, identity);

    // Setup data store with loaded configuration
    let config_store = config.store.as_ref();

    let verify_write = config_store
        .and_then(|store| store.verify_write)
        .unwrap_or_default();
    let read_only = access != RepositoryAccess::ReadWrite;

    let options =
        incremental_gc_options(read_only, global.no_gc() || global.dry_run(), config_store);

    // Create stores — disk-backed mutable store may need deferred upgrade
    let mut needs_upgrade = false;
    let (immutable_store, mutable_store) = if access == RepositoryAccess::NoStore {
        let (imm, mut_) = create_client_memory_stores().await?;
        (imm, mut_ as Arc<dyn MutableStore>)
    } else if global.in_memory() {
        if let Some(stores) = get_cached_in_memory_stores(&dot_path) {
            lore_debug!("Reusing cached in-memory stores");
            stores
        } else {
            let (imm, mut_) = create_client_memory_stores().await?;
            cache_in_memory_stores(dot_path.clone(), imm.clone(), mut_.clone());
            (imm, mut_)
        }
    } else {
        let immutable_store =
            create_client_immutable_store(&config, dot_path.as_path(), options, verify_write)
                .await?;

        let mutable_store =
            create_client_mutable_store(&config, dot_path.as_path(), immutable_store.clone())
                .await?;

        needs_upgrade = mutable_store.needs_upgrade();
        (immutable_store, mutable_store as Arc<dyn MutableStore>)
    };

    let filter = load_filter(path).unwrap_or_default();

    // Resolve the remote eagerly only when we need it for the mutable store upgrade.
    // Otherwise keep it pending so local-only commands never block on the connect.
    let resolved_remote_for_upgrade = if needs_upgrade {
        match &remote {
            Ok(shared) => Some(shared.clone().await),
            Err(err) => Some(Err(err.clone())),
        }
    } else {
        None
    };

    // Deferred mutable store upgrade — requires remote connection.
    // Drop all refs to the mutable store, run upgrade, then re-create to read back migrated data.
    let mutable_store = if needs_upgrade {
        lore_debug!("Upgrading local mutable store");
        let remote_conn = resolved_remote_for_upgrade
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .ok_or_else(|| {
                RepositoryError::internal(
                    "Unable to upgrade local store to current client, need a connection to server",
                )
            })?;

        // Drop existing mutable store so upgrade has sole ownership of on-disk data
        drop(mutable_store);

        // Create a temporary store just for the upgrade
        let upgrade_store =
            create_client_mutable_store(&config, dot_path.as_path(), immutable_store.clone())
                .await?;
        crate::store::mutable::upgrade(
            &upgrade_store,
            immutable_store.clone(),
            Some(remote_conn),
            repository,
        )
        .await
        .forward::<RepositoryError>("Failed to create local store")?;
        drop(upgrade_store);

        // Re-create to read back the migrated data
        let mutable_store =
            create_client_mutable_store(&config, dot_path.as_path(), immutable_store.clone())
                .await?;
        lore_debug!("Upgraded local mutable store");
        mutable_store as Arc<dyn MutableStore>
    } else {
        mutable_store
    };

    // If the mutable store upgrade already forced resolution, we can use the resolved
    // connection to refresh the config with the authoritative remote/identity values.
    if let Some(Ok(ref remote_conn)) = resolved_remote_for_upgrade {
        let remote_url = remote_conn.remote_url();
        let identity = remote_conn.identity();
        if !remote_url.is_empty()
            && (remote_url != config.remote_url.as_deref().unwrap_or_default()
                || identity != config.identity.as_deref().unwrap_or_default())
        {
            if !remote_url.is_empty() {
                config.remote_url = Some(remote_url.to_string());
            }
            if !identity.is_empty() {
                config.identity = Some(identity.to_string());
            }
            if matches!(access, RepositoryAccess::ReadWrite) {
                let _ = save_config(config_path.as_path(), &config).await;
            }
        }
    }

    // Recover or generate instance ID if the instance file was missing.
    // A zero instance_id means recovery is needed.
    let instance_id = if instance_id.is_zero() {
        let recovered = crate::instance::recover_instance_id(
            repository,
            mutable_store.clone(),
            immutable_store.clone(),
            &path.display().to_string(),
        )
        .await;
        let id = recovered.unwrap_or_else(|| {
            lore_debug!("No matching instance found, generating new instance ID");
            crate::instance::InstanceId::generate()
        });
        id.write_to_file(instance_path)
            .await
            .internal("Failed to write local repository state: failed to write instance ID")?;
        lore_debug!("Instance ID set to: {id}");
        id
    } else {
        instance_id
    };

    // Keep the remote pending so local-only commands finish without waiting on the
    // background connect. The upgrade path above already forced resolution when needed.
    let remote_state = match (resolved_remote_for_upgrade, remote) {
        (Some(resolved), _) => RemoteState::from_result(resolved),
        (None, Ok(remote_fut)) => RemoteState::Pending(remote_fut),
        (None, Err(err)) => RemoteState::from_result(Err(err)),
    };
    let repository = RepositoryContext::new_with_state(
        Some(path.to_path_buf()),
        immutable_store,
        mutable_store,
        repository,
        instance_id,
        remote_state,
        filter,
        format,
    );
    let repository = match repo_lock {
        Some(lock) => repository.with_repository_lock(lock),
        None => repository,
    };
    let repository = match write_token {
        Some(token) => repository.with_write_token(token),
        None => repository,
    };

    // For now all commands act on local storage by default
    // Commit command will look at the global flag and set this explicitly
    repository.set_disable_upload(true);

    let config_file = config.file.unwrap_or_default();
    repository.set_direct_file_write(config_file.direct_write.unwrap_or_default());
    repository.set_direct_file_io(config_file.direct_io.unwrap_or_default());
    repository.set_disable_cache(!global.cache());

    if global.local() {
        repository.set_disable_upload(true);
    }

    let repository = Arc::new(repository);

    // Instance registration and lazy anchor migrations only run in write-mode
    // contexts — read-only commands shouldn't mutate persistent state during
    // load_and_connect. A read-only invocation that hits a repository needing
    // registration/migration simply defers the work to the next write command.
    if repository.try_write_token().is_some() {
        // Register instance if not already present in the mutable store.
        // This covers both newly generated IDs and pre-existing instances
        // upgrading from a version before instance registration was added.
        let (instance_key, instance_key_type) =
            crate::instance::instance_key(repository.salt(), instance_id);
        let needs_registration = repository
            .read_mutable_store()
            .load(repository.id, instance_key, instance_key_type)
            .await
            .map_or(true, |h| h.is_zero());
        if needs_registration
            && let Err(err) = crate::instance::register_instance(
                &repository,
                instance_id,
                &path.display().to_string(),
            )
            .await
        {
            lore_warn!("Failed to register instance: {err}");
        }

        // Lazy migration: move file-based anchors to the mutable store.
        //
        // Order matters for crash safety: write the new keys, flush the mutable
        // store, then remove the old files. If a crash occurred after removing
        // a file but before the new keys reached disk, the anchor would be lost
        // entirely — file gone, mutable store unchanged. Keeping the old files
        // until the flush returns means a crash mid-migration is recoverable:
        // the next write-mode load reruns the migration from the file.
        let current_anchor_path = dot_path.join(crate::anchor::CURRENT);
        let staged_anchor_path = dot_path.join(crate::anchor::STAGED);
        let mut migrated_current = false;
        let mut migrated_staged = false;

        if current_anchor_path.exists() {
            let (revision, branch) = crate::anchor::deserialize_migrate_old(&current_anchor_path)
                .await
                .internal("Failed to deserialize repository anchor")?;
            crate::instance::store_current_anchor_branch(&repository, branch)
                .await
                .forward::<RepositoryError>("Failed to serialize repository anchor")?;
            crate::instance::store_current_anchor(&repository, revision)
                .await
                .forward::<RepositoryError>("Failed to serialize repository anchor")?;
            migrated_current = true;
        }
        if staged_anchor_path.exists() {
            match crate::anchor::deserialize_migrate_old(&staged_anchor_path).await {
                Ok((revision, _branch)) => {
                    match crate::instance::store_staged_anchor(&repository, revision).await {
                        Ok(()) => migrated_staged = true,
                        Err(err) => {
                            lore_warn!("Failed to migrate staged anchor revision: {err}");
                        }
                    }
                }
                Err(err) => {
                    lore_warn!("Failed to read file-based staged anchor for migration: {err}");
                }
            }
        }

        if migrated_current || migrated_staged {
            repository.flush(true).await?;
        }

        if migrated_current {
            if let Err(err) = tokio::fs::remove_file(&current_anchor_path).await {
                lore_warn!("Failed to remove old current anchor file: {err}");
            }
            lore_debug!("Migrated file-based current anchor to mutable store");
        }
        if migrated_staged {
            if let Err(err) = tokio::fs::remove_file(&staged_anchor_path).await {
                lore_warn!("Failed to remove old staged anchor file: {err}");
            }
            lore_debug!("Migrated file-based staged anchor to mutable store");
        }
    }

    Ok(repository)
}

pub const MAX_NAME_LEN: usize = 1000;
pub const MAX_DESCRIPTION_LEN: usize = 65536;

pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .find(|c: char| {
                !c.is_ascii_alphanumeric() && (c != '/') && (c != '-') && (c != '_') && (c != '.')
            })
            .is_none()
}

pub async fn create_local(
    path: &Path,
    token: &RepositoryWriteToken,
    repository: RepositoryId,
    default_branch: BranchId,
    default_branch_name: String,
    config: RepositoryConfig,
    no_tracking: bool,
) -> Result<Arc<RepositoryContext>, RepositoryError> {
    // Check both formats for pre-existence
    if path.join(DOT_URC).exists() || path.join(DOT_LORE).exists() {
        return Err(RepositoryError::from(RepositoryAlreadyExists {
            path: path.display().to_string(),
        }));
    }
    let format = RepositoryFormat::Lore;
    let dotpath = path.join(format.dot_dir());
    let idpath = dotpath.join(ID);

    let dotpath_display = dotpath.display().to_string();
    tokio::fs::create_dir_all(dotpath.as_path())
        .await
        .internal_with(|| {
            format!("Failed to create repository, unable to create directory {dotpath_display}")
        })?;

    let idpath_display = idpath.display().to_string();
    #[allow(clippy::disallowed_methods)] // Authorized repository ID writer.
    tokio::fs::write(idpath.as_path(), repository.data())
        .await
        .internal_with(|| {
            format!(
                "Failed to write local repository state in created tracking directory {idpath_display}"
            )
        })?;

    let instance_id = crate::instance::InstanceId::generate();
    let instance_path = dotpath.join(INSTANCE);
    instance_id
        .write_to_file(instance_path)
        .await
        .internal("Failed to write local repository state: failed to write instance ID")?;

    let in_memory = no_tracking || execution_context().globals().in_memory();
    let (immutable_store, mutable_store): (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) =
        if in_memory {
            let (imm, mut_) = create_client_memory_stores().await?;
            if !no_tracking {
                cache_in_memory_stores(dotpath.clone(), imm.clone(), mut_.clone());
            }
            (imm, mut_)
        } else {
            crate::shared_store::ensure_shared_store_for_repo(&config)
                .await
                .forward::<RepositoryError>("Failed to create shared store")?;

            // Setup the data stores - don't care about eviction/compaction in the new repository
            let immutable_store = create_client_immutable_store(
                &config,
                dotpath.as_path(),
                ImmutableStoreCreateOptions::none(),
                true, /* Local store only, no upstream so verify writes */
            )
            .await?;

            let mutable_store =
                create_client_mutable_store(&config, dotpath.as_path(), immutable_store.clone())
                    .await?;

            // New store — no data to migrate, needs_upgrade will be false
            (immutable_store, mutable_store as Arc<dyn MutableStore>)
        };

    // `create_local` is inherently a write operation — instance registration
    // and default-branch creation below both mutate the mutable store. The
    // caller-supplied token authorizes those writes and keeps the per-path
    // write mutex held for the duration of setup.
    let repository = Arc::new(
        RepositoryContext::new(
            Some(path.to_path_buf()),
            immutable_store,
            mutable_store,
            repository,
            instance_id,
            Err(ProtocolError::from(NoRemote)),
            Arc::default(),
            RepositoryFormat::Lore,
        )
        .with_write_token(token.share()),
    );

    // Register instance in the mutable store
    crate::instance::register_instance(&repository, instance_id, &path.display().to_string())
        .await
        .forward::<RepositoryError>("Failed to create local store")?;

    // Generate default branch. When using a shared global mutable store, the
    // branch may already exist from another instance — skip creation in that case.
    if branch::load_name_to_id(repository.clone(), default_branch_name.as_str())
        .await
        .is_err()
    {
        branch::create(
            repository.clone(),
            token,
            default_branch,
            default_branch_name.as_str(),
            branch::default_category(),
            config.identity.as_deref().unwrap_or_default(),
            util::time::timestamp(),
            vec![],
            false,
            false, /* No need to create linked repositories branches */
        )
        .await
        .forward::<RepositoryError>("Failed to create branch")?;
    }

    // Set the current branch so that subsequent commands know which branch
    // we are on, even though there are no commits yet (zero revision).
    crate::instance::store_current_anchor_branch(&repository, default_branch)
        .await
        .forward::<RepositoryError>("Failed to serialize repository anchor")?;

    // Serialize config
    let config_path = dotpath.join(CONFIG);
    save_config(config_path, &config).await?;

    repository.flush(true).await?;

    Ok(repository)
}

pub fn load_filter(root_path: &Path) -> Option<Arc<filter::Filter>> {
    let format = RepositoryFormat::detect(root_path);
    let mut ignore_path = root_path.join(format.ignore_file());

    // Both formats use .loreignore as the primary ignore file; fall back to
    // legacy .urcignore whenever .loreignore is not present.
    if !ignore_path.exists() {
        let fallback = root_path.join(DOT_URCIGNORE);
        if fallback.exists() {
            ignore_path = fallback;
        }
    }

    let view_path = root_path.join(format.dot_dir()).join(VIEW_FILTER);

    if let Ok(filter) = filter::load(&ignore_path, &view_path) {
        Some(Arc::new(filter))
    } else {
        None
    }
}

fn branch_switch_create_recurse(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    branch: BranchId,
    latest: Hash,
    remote_metadata: Hash,
    dry_run: bool,
) -> Pin<Box<dyn Future<Output = Result<(), RepositoryError>> + Send>> {
    Box::pin(async move {
        branch_switch_create(repository, &token, branch, latest, remote_metadata, dry_run).await
    })
}

async fn branch_switch_create(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    latest: Hash,
    remote_metadata: Hash,
    dry_run: bool,
) -> Result<(), RepositoryError> {
    let metadata = branch::load_metadata(repository.clone(), remote_metadata)
        .await
        .forward::<RepositoryError>("Failed to load branch metadata")?;
    let branch_name =
        branch::name(&metadata).forward::<RepositoryError>("Failed to load branch metadata")?;
    let branch_category = branch::default_category();
    let stack = branch::stack(&metadata);
    let parent = stack
        .first()
        .map(|parent| parent.branch)
        .unwrap_or_default();

    if !parent.is_zero() && !branch::exist_local(repository.clone(), parent).await {
        let remote = repository
            .remote()
            .await
            .forward::<RepositoryError>("Branch not found")?;
        let parent_status = branch::load_remote(remote, repository.id, parent)
            .await
            .forward::<RepositoryError>("Branch not found")?;
        branch_switch_create_recurse(
            repository.clone(),
            token.share(),
            parent,
            parent_status.latest,
            parent_status.metadata,
            dry_run,
        )
        .await?;
    }

    let user_id = execution_context().user_id().await;

    branch::create(
        repository.clone(),
        token,
        branch,
        branch_name,
        branch_category,
        user_id.as_str(),
        util::time::timestamp(),
        stack,
        dry_run,
        true, /* Create linked repositories branches */
    )
    .await
    .forward::<RepositoryError>("Failed to create branch")?;

    branch::store_latest(
        repository.clone(),
        branch,
        latest,
        BranchLatestStatus::Divergent,
    )
    .await
    .forward::<RepositoryError>("Failed to create branch")?;

    lore_debug!("Created branch {} at revision {}", branch_name, latest);

    Ok(())
}

#[derive(Clone, Default, Debug)]
pub struct BranchSwitchOptions {
    /// Optional revision signature
    pub signature: Option<String>,
    /// Keep last local latest revision, do not sync latest revision from remote
    pub local: bool,
    /// Reset local modified files to match the incoming revision
    pub reset: bool,
    /// Search nearest when matching revisions
    pub search_nearest: bool,
    /// Only update anchor tracking without modifying or verifying files
    pub bare: bool,
}

pub async fn branch_switch(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: String,
    options: BranchSwitchOptions,
) -> Result<Hash, RepositoryError> {
    let context = execution_context();
    let global = context.globals();

    let branch = branch::resolve(repository.clone(), branch.as_str())
        .await
        .forward::<RepositoryError>("Invalid branch")?;

    let branch_metadata = branch::metadata(repository.clone(), branch.id)
        .await
        .forward::<RepositoryError>("Invalid branch")?;
    let branch_name =
        branch::name(&branch_metadata).forward::<RepositoryError>("Invalid branch")?;

    let branch_stack = branch::stack(&branch_metadata);
    let branch_point = if branch_stack.is_empty() {
        Hash::default()
    } else {
        branch_stack[0].revision
    };

    let (branch_latest_local, branch_latest_remote, branch_location, branch_signature) = {
        let signature = if let Some(revision) = options.signature.as_ref() {
            let revision = revision::resolve(
                repository.clone(),
                revision,
                global.search_limit(),
                global.search_location(),
            )
            .await
            .forward::<RepositoryError>("Invalid revision")?;

            let state = state::State::deserialize(repository.clone(), revision)
                .await
                .forward::<RepositoryError>("Invalid revision")?;
            if state.branch(repository.clone()).await != branch.id && revision != branch_point {
                return Err(RepositoryError::internal(
                    "Given revision is not on the target branch",
                ));
            }

            revision
        } else {
            Hash::default()
        };

        lore_debug!(
            "Resolved signature {:?} to {}",
            options.signature,
            signature
        );

        let local_head = branch::load_latest(repository.clone(), branch.id)
            .await
            .ok();

        let remote_branch = if options.local {
            lore_debug!("Using local latest revision");
            None
        } else {
            match repository.remote().await {
                Ok(remote) => {
                    lore_debug!("Loading remote latest revision");
                    match branch::load_remote(remote, repository.id, branch.id).await {
                        Ok(remote_head) => Some(remote_head),
                        Err(err) if err.is_branch_not_found() => None,
                        Err(err) => {
                            if local_head.is_some() {
                                lore_debug!(
                                    "Failed to load remote branch latest revision, falling back to local: {err}"
                                );
                                None
                            } else {
                                return Err(err).forward::<RepositoryError>(
                                    "Failed to load remote branch latest revision",
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    if local_head.is_some() {
                        lore_debug!(
                            "Remote unavailable, switching using local branch state: {err}"
                        );
                        None
                    } else {
                        return Err(err).forward::<RepositoryError>(
                            "Failed to load remote branch latest revision",
                        );
                    }
                }
            }
        };

        if local_head.is_none() && remote_branch.is_none() {
            return Err(RepositoryError::from(BranchNotFound {
                branch: branch_name.to_string(),
            }));
        }

        if let Some(local_head) = local_head {
            let signature_if_local = if signature.is_zero() {
                local_head
            } else {
                signature
            };
            let (latest_local, latest_remote, location, signature) = if let Some(remote_status) =
                remote_branch
            {
                let remote_head = remote_status.latest;
                let signature_if_remote = if signature.is_zero() {
                    remote_head
                } else {
                    signature
                };
                // Check if remote is ahead of local and local is not diverged
                lore_debug!(
                    "Check if remote revision {remote_head} is ahead of local revision {local_head}"
                );
                if let Ok(remote_state) =
                    state::State::deserialize(repository.clone(), remote_head).await
                {
                    if let Ok(local_state) =
                        state::State::deserialize(repository.clone(), local_head).await
                    {
                        if remote_state.revision_number() > local_state.revision_number() {
                            // Check for divergence
                            lore_debug!(
                                "Remote revision {} is ahead of local revision {}, check for divergence",
                                remote_state.revision_number(),
                                local_state.revision_number()
                            );
                            if find::find_revision(
                                repository.clone(),
                                branch.id,
                                remote_head,
                                false,
                                None,
                                |state, _metadata| {
                                    if state.revision() == local_head
                                        || state.parent_other() == local_head
                                    {
                                        find::FindMatchResult::Match
                                    } else if state.revision_number()
                                        < local_state.revision_number()
                                    {
                                        // Divergence, the remote branch history passed the point
                                        // where local revision should have been found
                                        find::FindMatchResult::Abort
                                    } else {
                                        find::FindMatchResult::Continue
                                    }
                                },
                            )
                            .await
                            .is_ok()
                            {
                                lore_debug!("Branch is coherent, sync to remote LATEST");
                                (
                                    remote_head,
                                    remote_head,
                                    LoreBranchLocation::Remote,
                                    signature_if_remote,
                                )
                            } else {
                                lore_debug!("Branch is divergent, sync to local LATEST");
                                (
                                    local_head,
                                    remote_head,
                                    LoreBranchLocation::Local,
                                    signature_if_local,
                                )
                            }
                        } else if remote_state.revision() == local_state.revision() {
                            lore_debug!(
                                "Remote and local revision are equal, treat as remote sync"
                            );
                            (
                                remote_head,
                                remote_head,
                                LoreBranchLocation::Remote,
                                signature_if_remote,
                            )
                        } else {
                            lore_debug!("Local revision is ahead, sync to local latest");
                            (
                                local_head,
                                remote_head,
                                LoreBranchLocation::Local,
                                signature_if_local,
                            )
                        }
                    } else {
                        lore_debug!(
                            "Failed to load local latest revision state, sync to remote latest"
                        );
                        (
                            remote_head,
                            remote_head,
                            LoreBranchLocation::Remote,
                            signature_if_remote,
                        )
                    }
                } else {
                    lore_debug!(
                        "Failed to load remote latest revision state, sync to local latest"
                    );
                    (
                        local_head,
                        remote_head,
                        LoreBranchLocation::Local,
                        signature_if_local,
                    )
                }
            } else {
                lore_debug!("No remote branch available, sync to local latest");
                (
                    local_head,
                    Hash::default(),
                    LoreBranchLocation::Local,
                    signature_if_local,
                )
            };

            event::LoreEvent::BranchSwitchBegin(LoreBranchSwitchBeginEventData {
                branch: LoreBranchSwitchData::new(
                    branch.id,
                    branch_name,
                    latest_local,
                    latest_remote,
                    signature,
                    location,
                ),
            })
            .send();

            (latest_local, latest_remote, location, signature)
        } else {
            let Some(remote_status) = remote_branch else {
                return Err(RepositoryError::from(BranchNotFound {
                    branch: branch_name.to_string(),
                }));
            };

            let signature = if signature.is_zero() {
                remote_status.latest
            } else {
                signature
            };

            event::LoreEvent::BranchSwitchBegin(LoreBranchSwitchBeginEventData {
                branch: LoreBranchSwitchData::new(
                    branch.id,
                    branch_name,
                    remote_status.latest,
                    remote_status.latest,
                    signature,
                    LoreBranchLocation::Remote,
                ),
            })
            .send();

            branch_switch_create(
                repository.clone(),
                token,
                branch.id,
                remote_status.latest,
                remote_status.metadata,
                global.dry_run(),
            )
            .await?;

            (
                remote_status.latest,
                remote_status.latest,
                LoreBranchLocation::Remote,
                signature,
            )
        }
    };

    // Reject a switch that would discard an actually-staged change; dirty-only
    // tracking is carried forward by rebase_staged_anchor below. --force and
    // --reset intentionally discard the staged state instead.
    if !global.force()
        && !options.reset
        && let Some(staged_revision) = crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
        && !staged_revision.is_zero()
    {
        let state_staged = state::State::deserialize(repository.clone(), staged_revision)
            .await
            .forward::<RepositoryError>("Failed to deserialize staged state")?;
        if state_staged
            .node_has_staged_children(repository.clone(), crate::node::ROOT_NODE)
            .await
            .forward::<RepositoryError>("Failed to check staged nodes")?
        {
            return Err(InvalidArguments {
                reason: "Unable to switch branch when there is a staged state".into(),
            }
            .into());
        }
    }

    if !options.bare {
        // Synchronize state to the current local branch latest
        let sync_options = SyncOptions {
            revision: Some(branch_signature.to_string()),
            reset: options.reset,
            forward_changes: global.force(), /* Fast forward and stomp with local changes if forced */
            ..Default::default()
        };
        Box::pin(sync::sync(repository.clone(), token, sync_options))
            .await
            .forward::<RepositoryError>("Failed to synchronize state during branch switch")?;
    }

    if !global.dry_run() {
        branch::store_latest(
            repository.clone(),
            branch.id,
            branch_latest_local,
            if branch_location == LoreBranchLocation::Local {
                BranchLatestStatus::Divergent
            } else {
                BranchLatestStatus::Convergent
            },
        )
        .await
        .forward::<RepositoryError>("Failed to store the new latest revision for branch")?;

        // Warn if another instance has this branch checked out
        crate::instance::warn_branch_multiple_instance(&repository, branch.id).await;

        crate::instance::store_current_anchor_branch(&repository, branch.id)
            .await
            .forward::<RepositoryError>("Failed to serialize repository anchor")?;
        crate::instance::store_current_anchor(&repository, branch_signature)
            .await
            .forward::<RepositoryError>("Failed to serialize repository anchor")?;

        if global.force() {
            let _ = crate::instance::delete_staged_anchor(&repository).await;
        } else {
            state::rebase_staged_anchor(repository.clone(), branch_signature)
                .await
                .forward::<RepositoryError>("Failed to rebase staged anchor")?;
        }

        if branch_location == LoreBranchLocation::Remote {
            branch::store_last_sync(repository.clone(), branch.id, branch_latest_local).await;
        }

        if !options.bare {
            // Switch layers to follow the main branch
            layer_branch_switch(
                repository.clone(),
                token,
                branch.id,
                branch_name,
                branch_signature,
                options.reset,
            )
            .await?;
        }
    }

    event::LoreEvent::BranchSwitchEnd(LoreBranchSwitchEndEventData {
        branch: LoreBranchSwitchData::new(
            branch.id,
            branch_name,
            branch_latest_local,
            branch_latest_remote,
            branch_signature,
            branch_location,
        ),
    })
    .send();

    Ok(branch_latest_local)
}

fn layer_branch_name(branch_name: &str, layer_id: RepositoryId) -> String {
    format!("{}-{}", branch_name, &layer_id.to_string()[..8])
}

async fn layer_branch_switch(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch_id: BranchId,
    branch_name: &str,
    branch_signature: Hash,
    reset: bool,
) -> Result<(), RepositoryError> {
    let layers = layer::list(repository.clone())
        .await
        .forward::<RepositoryError>("Failed to switch branch in layer")?;

    if layers.is_empty() {
        return Ok(());
    }

    let context = execution_context();
    let global = context.globals();

    let mut layer_updates: Vec<(RepositoryId, String, Hash)> = Vec::new();

    for layer in layers {
        // Check for uncommitted staged changes in layer
        if !layer.staged.is_zero() && layer.staged != layer.current {
            if !global.force() {
                return Err(RepositoryError::internal(
                    "Layer has uncommitted staged changes, commit or use --force",
                ));
            }
            // Force mode: clear staged state
            layer::store_layer_staged(
                repository.clone(),
                token,
                layer.target_path.as_str(),
                layer.repository,
                Hash::default(),
            )
            .await
            .forward::<RepositoryError>("Failed to switch branch in layer")?;
        }

        let layer_repository = Arc::new(repository.to_layer_context(layer.repository).await);

        // Check if branch already exists in layer repo
        let branch_exists = branch::exist_local(layer_repository.clone(), branch_id).await;

        if !branch_exists {
            // Create branch in layer repo - mirrors layer_branch_create pattern
            let current_revision =
                state::State::deserialize(layer_repository.clone(), layer.current)
                    .await
                    .forward::<RepositoryError>("Failed to deserialize repository state")?;
            let current_branch = current_revision.branch(layer_repository.clone()).await;

            if current_branch == branch_id {
                // Already on this branch (by ID), skip
                continue;
            }

            let parent_metadata = branch::metadata(layer_repository.clone(), current_branch)
                .await
                .forward::<RepositoryError>("Failed to load branch metadata")?;
            let mut stack = branch::stack(&parent_metadata);
            stack.insert(
                0,
                BranchPoint {
                    branch: current_branch,
                    revision: current_revision.revision(),
                },
            );

            let user_id = execution_context().user_id().await;

            // Try creating with original name first
            match branch::create(
                layer_repository.clone(),
                token,
                branch_id,
                branch_name,
                branch::default_category(),
                user_id.as_str(),
                util::time::timestamp(),
                stack.clone(),
                false,
                true, /* Create linked repositories branches */
            )
            .await
            {
                Ok(_) => {
                    lore_debug!("Created branch {branch_name} in layer {}", layer.repository);
                }
                Err(err) if err.is_branch_already_exists() => {
                    // Name conflict — check if name maps to the same ID
                    if let Ok(existing_id) =
                        branch::load_name_to_id(layer_repository.clone(), branch_name).await
                    {
                        if existing_id != branch_id {
                            // Different ID, generate unique name with suffix and retry
                            let base_name = layer_branch_name(branch_name, layer.repository);
                            let mut attempt_name = base_name.clone();
                            let mut counter = 2u32;
                            loop {
                                match branch::create(
                                    layer_repository.clone(),
                                    token,
                                    branch_id,
                                    attempt_name.as_str(),
                                    branch::default_category(),
                                    user_id.as_str(),
                                    util::time::timestamp(),
                                    stack.clone(),
                                    false,
                                    true, /* Create linked repositories branches */
                                )
                                .await
                                {
                                    Ok(_) => {
                                        lore_debug!(
                                            "Created branch {attempt_name} in layer {} (name conflict)",
                                            layer.repository
                                        );
                                        break;
                                    }
                                    Err(err) if err.is_branch_already_exists() => {
                                        attempt_name = format!("{base_name}-{counter}");
                                        counter += 1;
                                        if counter > 10 {
                                            return Err(RepositoryError::internal(
                                                "Failed to switch branch in layer",
                                            ));
                                        }
                                    }
                                    Err(_) => {
                                        return Err(RepositoryError::internal(
                                            "Failed to switch branch in layer",
                                        ));
                                    }
                                }
                            }
                        }
                    } else {
                        return Err(RepositoryError::internal(
                            "Failed to switch branch in layer",
                        ));
                    }
                }
                Err(err) => {
                    lore_warn!(
                        "Failed to create branch in layer {}: {err}",
                        layer.repository
                    );
                    continue;
                }
            }
        }

        // Resolve target revision for the layer on the new branch
        let layer_latest = layer::latest_revision(layer_repository.clone(), branch_id)
            .await
            .unwrap_or_default();

        let layer_revision = if layer_latest.is_zero() {
            // No revision on this branch yet, keep current
            layer.current
        } else if layer.metadata.is_some() {
            // Metadata-based matching
            let state_target = state::State::deserialize(repository.clone(), branch_signature)
                .await
                .forward::<RepositoryError>("Failed to deserialize repository state")?;
            match layer::find_revision_match(
                repository.clone(),
                layer_repository.clone(),
                branch_id,
                state_target,
                layer_latest,
                layer.metadata.as_deref(),
            )
            .await
            {
                Ok((revision, _source)) => revision,
                Err(_) => layer_latest,
            }
        } else {
            layer_latest
        };

        // Sync layer files from current to target revision
        if layer_revision != layer.current {
            let target_path = RelativePath::new_from_initial_path(layer.target_path.as_str())
                .forward::<RepositoryError>("Failed to switch branch in layer")?;
            let source_path = RelativePath::new_from_initial_path(layer.source_path.as_str())
                .forward::<RepositoryError>("Failed to switch branch in layer")?;

            let layer_current = state::State::deserialize(layer_repository.clone(), layer.current)
                .await
                .forward::<RepositoryError>("Failed to deserialize repository state")?;
            let layer_target = state::State::deserialize(layer_repository.clone(), layer_revision)
                .await
                .forward::<RepositoryError>("Failed to deserialize repository state")?;

            let sync_options = SyncOptions {
                reset,
                ..Default::default()
            };

            if let Err(err) = Box::pin(layer::sync(
                layer_repository,
                layer_current,
                layer_target,
                target_path.clone(),
                source_path,
                sync_options,
            ))
            .await
            {
                lore_warn!("Failed to sync layer {}: {err}", layer.repository);
                continue;
            }

            layer_updates.push((layer.repository, layer.target_path.clone(), layer_revision));
        }
    }

    // Batch update all layer configs in a single read-modify-write cycle
    let batch: Vec<(RepositoryId, &str, Hash)> = layer_updates
        .iter()
        .map(|(repo, path, hash)| (*repo, path.as_str(), *hash))
        .collect();
    layer::store_layer_current_batch(repository.clone(), token, &batch)
        .await
        .forward::<RepositoryError>("Failed to switch branch in layer")?;

    Ok(())
}

pub const METADATA: &str = "repository-metadata";
pub const LIST: &str = "repository-list";

pub const NAME: &str = "name";
pub const DESCRIPTION: &str = "description";
pub const DEFAULT_BRANCH: &str = "default-branch";
pub const DEFAULT_BRANCH_NAME: &str = "default-branch-name";
pub const CREATOR: &str = "creator";
pub const CREATED: &str = "created";

#[derive(Default, Clone, Debug, PartialEq)]
pub struct RepositoryMetadata {
    pub name: String,
    pub description: String,
    pub default_branch: BranchId,
    pub default_branch_name: String,
    pub creator: String,
    pub created: u64,
}

fn mutable_key_type(function: &str) -> KeyType {
    match function {
        METADATA => KeyType::RepositoryMetadata,
        ID => KeyType::RepositoryId,
        _ => KeyType::Untyped,
    }
}

pub fn mutable_key(salt: &[u8], function: &str, repository: RepositoryId) -> (Hash, KeyType) {
    let key = hash::hash_function_arg(salt, function, hex::encode(repository.data()).as_str());
    let key_type = mutable_key_type(function);
    (key, key_type)
}

fn mutable_name_key(salt: &[u8], function: &str, name: &str) -> (Hash, KeyType) {
    let key = hash::hash_function_arg(salt, function, name);
    let key_type = mutable_key_type(function);
    (key, key_type)
}

pub async fn list_local(
    repository: Arc<RepositoryContext>,
) -> Result<impl tokio_stream::Stream<Item = Context>, RepositoryError> {
    let stream = repository
        .read_mutable_store()
        .list(RepositoryId::default(), KeyType::RepositoryId)
        .await
        .forward::<RepositoryError>("Failed to store repository metadata")?;

    Ok(UnboundedReceiverStream::new(stream.channel()).map(|(_, id)| id.to_context()))
}

pub async fn metadata_hash(repository: Arc<RepositoryContext>) -> Result<Hash, RepositoryError> {
    let (key, key_type) = mutable_key(repository.salt(), METADATA, repository.id);
    let result = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .forward::<RepositoryError>("Failed to load repository metadata");
    if result.is_ok() {
        return result;
    }

    if let Ok(remote) = repository.remote().await
        && let Ok(repository_service) = remote.repository().await
        && let Ok(response) = repository_service.query(Some(repository.id), None).await
    {
        let _ = metadata_store_hash(repository.clone(), response.metadata).await;
        return Ok(response.metadata);
    }

    result
}

pub async fn metadata_store(
    repository: Arc<RepositoryContext>,
    metadata: RepositoryMetadata,
) -> Result<Hash, RepositoryError> {
    let mut repository_metadata = Metadata::new();
    repository_metadata
        .set_string(NAME, metadata.name.as_str())
        .forward::<RepositoryError>("Failed to store repository metadata")?;
    repository_metadata
        .set_string(DESCRIPTION, metadata.description.as_str())
        .forward::<RepositoryError>("Failed to store repository metadata")?;
    repository_metadata
        .set_context(DEFAULT_BRANCH, metadata.default_branch)
        .forward::<RepositoryError>("Failed to store repository metadata")?;
    repository_metadata
        .set_string(DEFAULT_BRANCH_NAME, metadata.default_branch_name.as_str())
        .forward::<RepositoryError>("Failed to store repository metadata")?;
    repository_metadata
        .set_string(CREATOR, metadata.creator.as_str())
        .forward::<RepositoryError>("Failed to store repository metadata")?;
    repository_metadata
        .set_u64(CREATED, metadata.created)
        .forward::<RepositoryError>("Failed to store repository metadata")?;

    let metadata_hash = repository_metadata
        .serialize(repository)
        .await
        .forward::<RepositoryError>("Failed to store repository metadata")?;
    Ok(metadata_hash)
}

pub async fn metadata_store_hash(
    repository: Arc<RepositoryContext>,
    metadata: Hash,
) -> Result<(), RepositoryError> {
    let (key, key_type) = mutable_key(repository.salt(), METADATA, repository.id);
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| RepositoryError::from(WriteRequired))?;
    handle
        .store(repository.id, key, metadata, key_type)
        .await
        .forward::<RepositoryError>("Failed to store repository metadata")
}

pub async fn metadata(
    repository: Arc<RepositoryContext>,
    metadata: Hash,
) -> Result<RepositoryMetadata, RepositoryError> {
    let metadata = Metadata::deserialize(repository, metadata)
        .await
        .forward::<RepositoryError>("Repository not found")?;

    Ok(RepositoryMetadata {
        name: metadata
            .get_string(NAME)
            .forward::<RepositoryError>("Failed to deserialize repository metadata")?
            .to_string(),
        description: metadata
            .get_string(DESCRIPTION)
            .forward::<RepositoryError>("Failed to deserialize repository metadata")?
            .to_string(),
        default_branch: metadata
            .get_context(DEFAULT_BRANCH)
            .forward::<RepositoryError>("Failed to deserialize repository metadata")?,
        default_branch_name: metadata
            .get_string(DEFAULT_BRANCH_NAME)
            .forward::<RepositoryError>("Failed to deserialize repository metadata")?
            .to_string(),
        creator: metadata
            .get_string(CREATOR)
            .forward::<RepositoryError>("Failed to deserialize repository metadata")?
            .to_string(),
        created: metadata
            .get_u64(CREATED)
            .forward::<RepositoryError>("Failed to deserialize repository metadata")?,
    })
}

pub async fn store_name_to_id(
    repository: Arc<RepositoryContext>,
    name: impl AsRef<str>,
    id: RepositoryId,
) -> Result<(), RepositoryError> {
    // Store the name -> ID lookup
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name.as_ref());
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| RepositoryError::from(WriteRequired))?;
    handle
        .store(
            RepositoryId::default(),
            key,
            Hash::from_context(id.into()),
            key_type,
        )
        .await
        .forward::<RepositoryError>("Failed to store repository metadata")
}

pub async fn delete_name_to_id(
    repository: Arc<RepositoryContext>,
    name: impl AsRef<str>,
) -> Result<(), RepositoryError> {
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name.as_ref());
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| RepositoryError::from(WriteRequired))?;
    handle
        .store(RepositoryId::default(), key, Hash::default(), key_type)
        .await
        .forward::<RepositoryError>("Failed to store repository metadata")
}

pub async fn id_from_name(
    repository: Arc<RepositoryContext>,
    name: impl AsRef<str>,
) -> Result<RepositoryId, RepositoryError> {
    let name = name.as_ref();
    let (key, key_type) = mutable_name_key(repository.salt(), ID, name);
    let id = repository
        .read_mutable_store()
        .load(RepositoryId::default(), key, key_type)
        .await
        .forward::<RepositoryError>("Repository not found")?;
    Ok(RepositoryId::from(id.to_context()))
}

pub fn repository_id(repository_path: impl AsRef<str>) -> Result<RepositoryId, RepositoryError> {
    let Ok(path) = util::path::make_absolute(repository_path) else {
        return Err(RepositoryError::internal("Invalid repository path"));
    };

    let dot_path = path.join(RepositoryFormat::detect(&path).dot_dir());
    let id_path = dot_path.join(ID);

    Ok(read_id_from_file(id_path).internal("Repository not found")?)
}

pub fn repository_remote(repository_path: impl AsRef<str>) -> Result<String, RepositoryError> {
    let Ok(path) = util::path::make_absolute(repository_path) else {
        return Err(RepositoryError::internal("Invalid repository path"));
    };

    let dot_path = path.join(RepositoryFormat::detect(&path).dot_dir());
    let config_path = dot_path.join(CONFIG);

    let Ok(config) = load_config(config_path) else {
        return Err(RepositoryError::internal("Invalid repository path"));
    };

    Ok(config.remote_url.unwrap_or_default())
}

pub async fn gc(repository: Arc<RepositoryContext>) -> Result<(), RepositoryError> {
    let dot_path = repository.require_path()?.join(repository.format.dot_dir());
    let config_path = dot_path.join(CONFIG);

    let config = load_config(config_path.as_path())?;
    let config_store = config.store.as_ref();

    let sync_data = crate::runtime::try_execution_context()
        .is_some_and(|context| context.globals().sync_data());
    crate::store::gc(
        repository.immutable_store(),
        config_store
            .map(|config| config.max_capacity.unwrap_or_default())
            .unwrap_or_default(),
        config_store
            .map(|config| config.max_size.unwrap_or_default())
            .unwrap_or_default(),
        sync_data,
        gc_event_sink(),
    )
    .await;

    Ok(())
}

// TODO(vri): Upgrade and return connection
pub async fn resolve_by_name(
    remote_url: &str,
    name: &str,
    identity: &str,
) -> Result<RepositoryData, RepositoryError> {
    let connection = protocol::connect(
        remote_url,
        identity,
        RepositoryId::default(), /* No repository */
    )
    .await
    .forward_with::<RepositoryError, _>(|| {
        format!("Failed to connect to remote repository {remote_url}")
    })?;

    let repository_service = connection
        .repository()
        .await
        .forward_with::<RepositoryError, _>(|| {
            format!("Failed to connect to remote repository {remote_url}")
        })?;

    match repository_service.query(None, Some(name)).await {
        Ok(data) => Ok(data),
        Err(err) if err.is_not_found() => {
            // Fallback path in case it's an ID
            let id = RepositoryId::from_str(name).unwrap_or_default();
            if id.is_zero() {
                return Err(RepositoryError::from(NotFound));
            }
            repository_service
                .query(Some(id), None)
                .await
                .forward::<RepositoryError>("Repository not found")
        }
        Err(err) => Err(err).forward::<RepositoryError>("Repository not found"),
    }
}

#[cfg(test)]
// These tests spawn tokio tasks directly without a LORE_CONTEXT, which is fine for
// state-machine unit tests that don't touch the execution context.
#[allow(clippy::disallowed_methods)]
mod remote_state_tests {
    //! Tests for the `RemoteState` state machine and the `RepositoryContext::remote()`
    //! lazy resolution path. These exercise the classification logic and the Pending →
    //! terminal-state promotion without requiring a real `Arc<Connection>` (Connection
    //! construction is non-trivial and covered by integration tests instead). We cover:
    //!
    //! - Classification: `RemoteState::from_result` routes each error variant correctly.
    //! - Terminal-state passthrough: `remote()` on Offline/Failed returns the expected
    //!   result via the read-lock fast path.
    //! - Pending resolution: `remote()` awaits the shared future and promotes the state.
    //! - Concurrent awaiters: N tasks awaiting the same Pending converge on one result.
    //! - Cancellation: cancelling awaiters mid-await does not break subsequent awaiters
    //!   or the promotion.
    use std::sync::Arc;

    use futures::FutureExt;
    use futures::future::BoxFuture;
    use lore_transport::ProtocolError;

    use super::RemoteFuture;
    use super::RemoteState;
    use super::RepositoryContext;
    use crate::errors::Disconnected;
    use crate::errors::NoRemote;
    use crate::lore::RepositoryId;

    fn disconnected() -> ProtocolError {
        ProtocolError::from(Disconnected)
    }

    fn no_remote() -> ProtocolError {
        ProtocolError::from(NoRemote)
    }

    /// Build a `RemoteFuture` that resolves to the given result, without going through
    /// a real connect or spawning a task.
    fn ready_remote(
        result: Result<Arc<lore_transport::Connection>, ProtocolError>,
    ) -> RemoteFuture {
        let fut: BoxFuture<'static, _> = async move { result }.boxed();
        fut.shared()
    }

    /// Build a minimal `RepositoryContext` carrying the given `RemoteState`. We construct
    /// in-memory stores so the rest of the context is valid, but only `remote()` is
    /// exercised.
    async fn context_with_state(state: RemoteState) -> Arc<RepositoryContext> {
        let (immutable, mutable) = super::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        Arc::new(RepositoryContext::new_with_state(
            None,
            immutable,
            mutable,
            RepositoryId::default(),
            crate::instance::InstanceId::default(),
            state,
            Arc::default(),
            crate::repository::RepositoryFormat::Lore,
        ))
    }

    #[tokio::test]
    async fn from_result_classifies_no_remote_as_offline() {
        let state = RemoteState::from_result(Err(no_remote()));
        assert!(matches!(state, RemoteState::Offline));
    }

    #[tokio::test]
    async fn from_result_classifies_other_errors_as_failed() {
        let state = RemoteState::from_result(Err(disconnected()));
        assert!(matches!(state, RemoteState::Failed(_)));
    }

    #[tokio::test]
    async fn remote_returns_no_remote_for_offline_state() {
        let ctx = context_with_state(RemoteState::Offline).await;
        let result = ctx.remote().await;
        assert!(matches!(result, Err(ProtocolError::NoRemote(_))));
    }

    #[tokio::test]
    async fn remote_returns_original_error_for_failed_state() {
        let ctx = context_with_state(RemoteState::Failed(disconnected())).await;
        let result = ctx.remote().await;
        assert!(matches!(result, Err(ProtocolError::Disconnected(_))));
    }

    #[tokio::test]
    async fn pending_err_transitions_to_failed() {
        let ctx = context_with_state(RemoteState::Pending(ready_remote(Err(disconnected())))).await;

        // First call drives resolution.
        let result = ctx.remote().await;
        assert!(matches!(result, Err(ProtocolError::Disconnected(_))));

        // State should now be promoted to terminal Failed — subsequent calls take the
        // fast path (no Pending match in the read-lock branch).
        let state = ctx.remote.read().await;
        assert!(
            matches!(*state, RemoteState::Failed(_)),
            "state should be promoted to Failed after Pending resolution"
        );
    }

    #[tokio::test]
    async fn pending_no_remote_transitions_to_offline() {
        let ctx = context_with_state(RemoteState::Pending(ready_remote(Err(no_remote())))).await;

        let result = ctx.remote().await;
        assert!(matches!(result, Err(ProtocolError::NoRemote(_))));

        let state = ctx.remote.read().await;
        assert!(
            matches!(*state, RemoteState::Offline),
            "NoRemote resolution should promote to Offline rather than Failed"
        );
    }

    #[tokio::test]
    async fn concurrent_awaiters_converge_on_single_resolution() {
        // Use a future that resolves after a tick, so all spawned tasks have a chance
        // to race on the Pending state before resolution completes.
        let slow: BoxFuture<'static, _> = async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            Err::<Arc<lore_transport::Connection>, _>(disconnected())
        }
        .boxed();
        let shared = slow.shared();
        let ctx = context_with_state(RemoteState::Pending(shared)).await;

        // Spawn many concurrent callers. All should get the same error result.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let ctx = ctx.clone();
            handles.push(tokio::spawn(async move { ctx.remote().await }));
        }
        for h in handles {
            let result = h.await.expect("task should not panic");
            assert!(matches!(result, Err(ProtocolError::Disconnected(_))));
        }

        // State should be promoted exactly once to the terminal result.
        let state = ctx.remote.read().await;
        assert!(matches!(*state, RemoteState::Failed(_)));
    }

    #[tokio::test]
    async fn remote_status_reports_offline() {
        let ctx = context_with_state(RemoteState::Offline).await;
        assert!(matches!(
            ctx.remote_status().await,
            super::RemoteStatus::Offline
        ));
    }

    #[tokio::test]
    async fn remote_status_reports_failed() {
        let ctx = context_with_state(RemoteState::Failed(disconnected())).await;
        assert!(matches!(
            ctx.remote_status().await,
            super::RemoteStatus::Failed(ProtocolError::Disconnected(_))
        ));
    }

    #[tokio::test]
    async fn remote_status_reports_pending_without_driving_connect() {
        // Use a future that would panic if polled, to prove remote_status never polls
        // the shared future.
        let never_poll: BoxFuture<'static, _> = async {
            panic!("remote_status must not poll the pending future");
        }
        .boxed();
        let ctx = context_with_state(RemoteState::Pending(never_poll.shared())).await;

        assert!(matches!(
            ctx.remote_status().await,
            super::RemoteStatus::Pending
        ));
        // State must still be Pending — remote_status must not promote.
        assert!(matches!(*ctx.remote.read().await, RemoteState::Pending(_)));
    }

    #[tokio::test]
    async fn cancelled_awaiter_does_not_break_subsequent_callers() {
        // Resolution waits for a signal so we can reliably cancel awaiters before it fires.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let gated: BoxFuture<'static, _> = async move {
            let _ = rx.await;
            Err::<Arc<lore_transport::Connection>, _>(disconnected())
        }
        .boxed();
        let shared = gated.shared();
        let ctx = context_with_state(RemoteState::Pending(shared)).await;

        // First caller registers a waker on the Shared future, then is cancelled.
        let ctx_for_cancel = ctx.clone();
        let cancelled = tokio::spawn(async move { ctx_for_cancel.remote().await });
        tokio::task::yield_now().await;
        cancelled.abort();
        let _ = cancelled.await;

        // Drive resolution.
        tx.send(())
            .expect("receiver should still be alive via the Shared future");

        // A fresh caller should still get the resolved error and see the state promoted.
        let result = ctx.remote().await;
        assert!(matches!(result, Err(ProtocolError::Disconnected(_))));
        let state = ctx.remote.read().await;
        assert!(matches!(*state, RemoteState::Failed(_)));
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod write_token_tests {
    //! Regression coverage for `clone --no-tracking`: a `NoStore` context that
    //! attaches the outer `Client` write token (held by clone for cross-thread
    //! exclusion on the destination path) must grant write capability via
    //! `try_write_mutable_store`. Without this, `branch::create`'s internal
    //! helpers (`store_name_to_id`, `metadata_store`, `store_latest`) fail
    //! with `WriteRequired`.
    use std::sync::Arc;

    use lore_transport::ProtocolError;

    use super::RepositoryContext;
    use super::RepositoryWriteToken;
    use crate::errors::NoRemote;
    use crate::lore::RepositoryId;

    async fn in_memory_context() -> Arc<RepositoryContext> {
        let (immutable, mutable) = super::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        Arc::new(RepositoryContext::new(
            None,
            immutable,
            mutable,
            RepositoryId::default(),
            crate::instance::InstanceId::default(),
            Err(ProtocolError::from(NoRemote)),
            Arc::default(),
            crate::repository::RepositoryFormat::Lore,
        ))
    }

    /// A `NoStore` context with a `Client` write token attached must grant
    /// write capability. This is the path `clone --no-tracking` takes: it
    /// holds the per-path mutex via the outer Client token (clone.rs:877)
    /// and shares siblings to every constructed context.
    #[tokio::test]
    async fn no_store_context_with_client_token_grants_write_capability() {
        let temp_dir =
            std::env::temp_dir().join(format!("lore-write-token-test-{}", std::process::id()));
        let token = RepositoryWriteToken::acquire(&temp_dir).await;
        let ctx = in_memory_context().await;
        let with_token = Arc::new(
            Arc::try_unwrap(ctx)
                .expect("sole owner")
                .with_write_token(token),
        );
        assert!(
            with_token.try_write_mutable_store().is_some(),
            "context with Client token should expose a write handle"
        );
    }

    /// Without an attached token, an in-memory context is read-only — confirms
    /// that `repository_call_no_store` callers (e.g. `config_get`) keep their
    /// fail-loud behavior on accidental writes.
    #[tokio::test]
    async fn no_token_means_no_write_capability() {
        let ctx = in_memory_context().await;
        assert!(
            ctx.try_write_mutable_store().is_none(),
            "context without a token must not expose a write handle"
        );
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod path_optional_tests {
    //! Coverage for the path-less `RepositoryContext` construction path used
    //! by the in-memory revision-tree surface. The context's `path` field is
    //! optional; when constructed with `None`, the context is fully usable
    //! for store-backed operations but `require_path` rejects callers that
    //! need a working-tree path.
    use std::sync::Arc;

    use lore_transport::ProtocolError;

    use super::RepositoryContext;
    use crate::errors::NoRemote;
    use crate::lore::RepositoryId;

    #[tokio::test]
    async fn require_path_returns_invalid_arguments_when_path_is_none() {
        let (immutable, mutable) = super::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        let ctx = RepositoryContext::new(
            None,
            immutable,
            mutable,
            RepositoryId::default(),
            crate::instance::InstanceId::default(),
            Err(ProtocolError::from(NoRemote)),
            Arc::default(),
            crate::repository::RepositoryFormat::Lore,
        );
        ctx.require_path()
            .expect_err("path-less context should reject require_path");
    }

    #[tokio::test]
    async fn require_path_returns_path_when_set() {
        let (immutable, mutable) = super::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        let path = std::path::PathBuf::from("/tmp/lore-test-require-path");
        let ctx = RepositoryContext::new(
            Some(path.clone()),
            immutable,
            mutable,
            RepositoryId::default(),
            crate::instance::InstanceId::default(),
            Err(ProtocolError::from(NoRemote)),
            Arc::default(),
            crate::repository::RepositoryFormat::Lore,
        );
        let got = ctx
            .require_path()
            .expect("path-bearing context should return path");
        assert_eq!(got, path.as_path());
    }
}
