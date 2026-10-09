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
use lore_transport::SessionPool;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
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
use crate::fs::swfs::mount_manager_state::MountManagerState;
use crate::global::GlobalConfig;
use crate::hash;
use crate::instance::InstanceId;
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
use crate::state::CanReadRepository;
use crate::state::allow_all_repositories;
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchSwitchBeginEventData {
    /// Details of the branch being switched to.
    pub branch: LoreBranchSwitchData,
}

/// Data for the event emitted when a branch switch finishes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchSwitchEndEventData {
    /// Details of the branch that was switched to.
    pub branch: LoreBranchSwitchData,
}

/// Data for the event emitted when a repository dump starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryDumpBeginEventData {
    /// Repository identifier.
    pub repository: RepositoryId,
    /// Revision being dumped.
    pub revision: Hash,
}

/// Data for the event emitted when a repository dump finishes.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryDumpEndEventData {
    /// Placeholder field. The event carries no data.
    pub _unused: u32,
}

/// Data for the event emitted when a repository configuration value is read.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
    pub vfs: Option<VfsConfig>,
}

impl RepositoryConfig {
    pub fn validate(&self) -> Result<(), RepositoryError> {
        if self.is_swfs()
            && !matches!(
                self.shared_store_to_use.as_ref(),
                Some(SharedStoreToUseConfig {
                    use_shared_store: Some(true),
                    ..
                })
            )
        {
            return Err(RepositoryError::internal(
                "Using SWFS without using a shared store",
            ));
        }
        Ok(())
    }

    pub fn is_swfs(&self) -> bool {
        self.vfs
            .as_ref()
            .is_some_and(|vfs_config| vfs_config.vfs_type.is_swfs())
    }
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
    #[lore_macro::test_pub]
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

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub enum VfsType {
    #[default]
    None,
    Swfs,
}

impl VfsType {
    pub fn is_swfs(&self) -> bool {
        matches!(self, VfsType::Swfs)
    }
}

#[error_set]
pub enum VfsConfigError {}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct VfsConfig {
    pub vfs_type: VfsType,
}

/// Decides whether the automatic incremental background GC (evictor + compactor) is
/// spawned for a freshly opened store, returning the caps to spawn it with or
/// [`ImmutableStoreCreateOptions::none`] to leave it unspawned.
///
/// Incremental GC is the default on write operations (`!read_only`). It is
/// suppressed by `--no-gc` and on dry-run commands (`suppress_incremental`, which
/// the caller folds together), since a dry run adds no content to collect and its
/// background tasks would otherwise race its own teardown. Read-only opens never
/// spawn it. When enabled but the repository has no `[store]` config, the built-in
/// [`StoreConfig::client_default`] caps apply.
#[lore_macro::test_pub]
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileConfig {
    direct_write: Option<bool>,
    flush_write: Option<bool>,
}

impl Default for FileConfig {
    fn default() -> Self {
        FileConfig {
            direct_write: Some(false),
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

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// Whether a repository being created or cloned should be backed by a shared store.
///
/// `Inherit` is zero so a zero-initialized C struct keeps following the machine's
/// `use_shared_store_automatically` setting, as callers have always relied on. `Disabled` exists
/// because that inherited setting is otherwise unconditional: without it, a caller on a machine
/// that opts in automatically has no way to ask for a repository backed by its own store.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, bitcode::Encode, bitcode::Decode)]
pub enum LoreSharedStoreMode {
    /// Follow the machine's `use_shared_store_automatically` global setting.
    #[default]
    Inherit = 0,
    /// Always back the repository with a shared store.
    Enabled = 1,
    /// Never back the repository with a shared store, whatever the global config says.
    Disabled = 2,
}

lore_base::carries_no_text!(LoreSharedStoreMode);

impl SharedStoreToUseConfig {
    pub fn from_api_args(
        global_config: &GlobalConfig,
        use_shared_store: LoreSharedStoreMode,
        path: &LoreString,
    ) -> Result<Option<SharedStoreToUseConfig>, PathError> {
        let enabled = match use_shared_store {
            LoreSharedStoreMode::Disabled => false,
            LoreSharedStoreMode::Enabled => true,
            LoreSharedStoreMode::Inherit => global_config
                .use_shared_store_automatically
                .unwrap_or(false),
        };
        if enabled {
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
}

impl Default for RepositoryRuntimeSettings {
    fn default() -> Self {
        RepositoryRuntimeSettings {
            disable_upload: AtomicBool::new(true),
            disable_cache: AtomicBool::new(true),
            direct_file_write: AtomicBool::new(false),
        }
    }
}

impl Clone for RepositoryRuntimeSettings {
    fn clone(&self) -> Self {
        RepositoryRuntimeSettings {
            disable_upload: AtomicBool::new(self.disable_upload.load(Ordering::Relaxed)),
            disable_cache: AtomicBool::new(self.disable_cache.load(Ordering::Relaxed)),
            direct_file_write: AtomicBool::new(self.direct_file_write.load(Ordering::Relaxed)),
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
/// - [`InMemory`](Self::InMemory): carries nothing, for the same reason as
///   `Server` — a path-less context has no per-path mutex to take, and the
///   only writes it makes are content-addressed store writes and the branch
///   tip, which the tip compare-and-swap serializes. Gated by
///   [`InMemoryContext`].
///
/// Tokens are stored as `Option<…>` on `RepositoryContext`; leaf write
/// sites fetch via `repository.try_write_token().ok_or(WriteRequired)?`.
#[must_use]
pub enum RepositoryWriteToken {
    Client(Arc<tokio::sync::OwnedMutexGuard<()>>),
    Server,
    InMemory,
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

    /// Mint a token for a path-less in-memory context. No mutex is taken —
    /// there is no working-tree path to key one on.
    ///
    /// Gated by [`InMemoryContext`] on the same principle as
    /// [`server`](Self::server): a crate opts in by implementing the marker
    /// for one of its own types, so skipping the per-path mutex is a
    /// compile-time decision rather than a call-site one.
    pub fn in_memory<C: InMemoryContext>(_: &C) -> Self {
        Self::InMemory
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
            Self::InMemory => Self::InMemory,
        }
    }
}

/// Marker trait that gates [`RepositoryWriteToken::in_memory`].
///
/// The counterpart to [`ServerContext`] for path-less in-memory contexts. The
/// trait body is empty — it exists purely to make "I am an in-memory context" a
/// compile-time prerequisite for skipping the per-path write mutex.
pub trait InMemoryContext {}

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
#[lore_macro::test_pub]
type RemoteFuture = Shared<BoxFuture<'static, Result<Arc<Connection>, ProtocolError>>>;

/// State machine for the remote connection. Lives behind `Arc<RwLock<_>>` so related
/// contexts (e.g. filter views) can share the same underlying connection state while
/// link/layer contexts build their own against a freshly-connected module.
#[lore_macro::test_pub]
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

#[derive(Debug, Clone)]
pub struct RepositoryPaths {
    path: PathBuf,
    dot_path: PathBuf,
}

/// Refuses a working-copy root that is not valid text.
///
/// Every path Lore reports, resolves from a user argument or keys a cache on is built from
/// this root, and each of those needs one spelling that survives a round trip. A root
/// without one could only be spelled approximately, so a path parsed back from a report
/// could name a different file than the one on disk. Refusing once, where a working copy is
/// opened or created, is what lets every path built from it be spelled losslessly.
pub fn require_text_root(path: &Path) -> Result<(), RepositoryError> {
    if path.to_str().is_some() {
        return Ok(());
    }
    Err(RepositoryError::from(InvalidPath {
        path: path.to_string_lossy().into_owned(),
    }))
}

impl RepositoryPaths {
    pub fn new(path: PathBuf, dot_path: PathBuf) -> Self {
        Self { path, dot_path }
    }
}

#[lore_macro::test_pub]
pub struct RepositoryContext {
    /// Working-tree path for this repository. `None` for path-less contexts
    /// (server-side handlers, in-memory revision-tree handles) that operate
    /// only on the underlying stores. Code that walks the working tree calls
    /// [`RepositoryContext::require_path`] to fail with
    /// [`RepositoryError::InvalidArguments`] when the path is absent.
    pub paths: Option<RepositoryPaths>,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    file_system: Arc<dyn FilesystemProvider>,
    pub id: RepositoryId,
    /// The root top level repository ID.
    root_id: RepositoryId,
    pub instance_id: crate::instance::InstanceId,
    remote: Arc<tokio::sync::RwLock<RemoteState>>,
    pub filter: Arc<Filter>,
    /// Defaults to allowing every repository; server handlers replace it with
    /// the requester's authorization.
    link_read: CanReadRepository,
    settings: RepositoryRuntimeSettings,
    is_link: bool,
    is_layer: bool,
    write_token: Option<RepositoryWriteToken>,
    repo_lock: Option<Arc<RepositoryLock>>,
    /// The storage session pool this repository's reads and writes pick from,
    /// resolved on first use.
    ///
    /// Weak on purpose. The strong reference lives in the connection's session
    /// cache, and `StorageSession::invalidate` clears that cache when the server
    /// reports a stale session id — after a reconnect that rotated its session
    /// map, say. An invalidated pool then stops upgrading here, so the next caller
    /// resolves a fresh one rather than handing out sessions the server has
    /// forgotten.
    session_pool: parking_lot::RwLock<Weak<SessionPool>>,
    /// The lazy storage session this context's reads and writes carry, built on first
    /// use. Held strongly: a pending session re-resolves after invalidation, so one
    /// serves every operation against this context's partition for its lifetime.
    lazy_session: parking_lot::RwLock<Option<Arc<lore_transport::StorageSession>>>,
}

impl std::fmt::Debug for RepositoryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RepositoryContext({})", self.id)
    }
}

impl RemoteState {
    /// Classify a resolved connect result into a terminal state. A `NoRemote` error
    /// reflects "no remote configured" rather than a failure, so it becomes `Offline`.
    #[lore_macro::test_pub]
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

pub struct RepositoryContextCreationArgs {
    pub paths: Option<RepositoryPaths>,
    pub immutable_store: Arc<dyn ImmutableStore>,
    pub mutable_store: Arc<dyn MutableStore>,
    pub id: RepositoryId,
    pub instance_id: crate::instance::InstanceId,
    pub remote: Result<Arc<Connection>, ProtocolError>,
    pub filter: Arc<Filter>,
    pub filesystem_provider: Option<Arc<dyn FilesystemProvider>>,
}

impl RepositoryContext {
    pub fn new(create_args: RepositoryContextCreationArgs) -> Self {
        Self::new_with_state(
            create_args.paths,
            create_args.immutable_store,
            create_args.mutable_store,
            create_args.id,
            create_args.instance_id,
            RemoteState::from_result(create_args.remote),
            create_args.filter,
            create_args.filesystem_provider,
        )
    }

    #[lore_macro::test_pub]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_state(
        paths: Option<RepositoryPaths>,
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
        id: RepositoryId,
        instance_id: crate::instance::InstanceId,
        remote: RemoteState,
        filter: Arc<Filter>,
        filesystem_provider: Option<Arc<dyn FilesystemProvider>>,
    ) -> Self {
        let file_system = filesystem_provider.unwrap_or_else(|| {
            Self::default_filesystem(
                paths
                    .as_ref()
                    .map_or(Path::new(""), |paths| paths.path.as_ref()),
            )
        });
        RepositoryContext {
            link_read: allow_all_repositories(),
            paths,
            immutable_store,
            mutable_store,
            id,
            root_id: id,
            instance_id,
            remote: remote_arc(remote),
            filter,
            settings: RepositoryRuntimeSettings::default(),
            is_link: false,
            is_layer: false,
            write_token: None,
            repo_lock: None,
            session_pool: Default::default(),
            lazy_session: Default::default(),
            file_system,
        }
    }

    /// Propagates into every context derived from this one.
    pub fn with_link_read(mut self, link_read: CanReadRepository) -> Self {
        self.link_read = link_read;
        self
    }

    pub fn can_read_link(&self, id: RepositoryId) -> bool {
        (self.link_read)(id)
    }

    pub fn path(&self) -> Option<&Path> {
        self.paths.as_ref().map(|paths| paths.path.as_ref())
    }

    /// Borrow the working-tree path required by filesystem-backed operations.
    /// Returns [`crate::errors::InvalidArguments`] for path-less contexts
    /// (server-side handlers, in-memory revision-tree handles); see the
    /// `path` field's documentation for the contract. Returning the FFI
    /// error directly lets `?` propagate into any caller `error_set` that
    /// carries an `InvalidArguments` variant.
    pub fn require_path(&self) -> Result<&Path, crate::errors::InvalidArguments> {
        self.path().ok_or_else(|| crate::errors::InvalidArguments {
            reason: "repository context has no working-tree path".to_string(),
        })
    }

    pub fn dot_dir_path(&self) -> Result<&Path, crate::errors::InvalidArguments> {
        if let Some(paths) = &self.paths {
            Ok(paths.dot_path.as_ref())
        } else {
            Err(InvalidArguments {
                reason: "repository context has no dot lore path".to_string(),
            })
        }
    }

    /// Display the working-tree path for logging and error messages. Renders
    /// `<unset>` for path-less contexts so log lines remain readable when the
    /// working tree is intentionally absent.
    pub fn path_for_display(&self) -> std::path::Display<'_> {
        self.path()
            .unwrap_or_else(|| Path::new("<unset>"))
            .display()
    }

    /// The root top level repository ID.
    pub fn root_id(&self) -> RepositoryId {
        self.root_id
    }

    pub fn salt(&self) -> &'static [u8] {
        SALT_LORE
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
            link_read: allow_all_repositories(),
            file_system: Self::default_filesystem(Path::new("")),
            paths: None,
            immutable_store,
            mutable_store,
            id,
            root_id: id,
            instance_id: crate::instance::InstanceId::default(),
            remote: remote_arc(RemoteState::Offline),
            filter: Arc::default(),
            settings: RepositoryRuntimeSettings::default(),
            is_link: false,
            is_layer: false,
            write_token: Some(RepositoryWriteToken::server(&INTERNAL_SERVER_CONTEXT)),
            repo_lock: None,
            session_pool: Default::default(),
            lazy_session: Default::default(),
        }
    }

    pub fn to_server_context(&self, id: RepositoryId) -> Self {
        RepositoryContext {
            link_read: self.link_read.clone(),
            paths: self.paths.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id,
            root_id: id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::Offline),
            filter: self.filter.clone(),
            settings: self.settings.clone(),
            is_link: false,
            is_layer: false,
            write_token: Some(RepositoryWriteToken::server(&INTERNAL_SERVER_CONTEXT)),
            repo_lock: None,
            session_pool: Default::default(),
            lazy_session: Default::default(),
            file_system: self.file_system.clone(),
        }
    }

    pub fn new_null_context(
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> Self {
        RepositoryContext {
            link_read: allow_all_repositories(),
            file_system: Self::default_filesystem(Path::new("")),
            paths: None,
            immutable_store,
            mutable_store,
            id: RepositoryId::default(),
            root_id: RepositoryId::default(),
            instance_id: crate::instance::InstanceId::default(),
            remote: remote_arc(RemoteState::Offline),
            filter: Arc::default(),
            settings: RepositoryRuntimeSettings::default(),
            is_link: false,
            is_layer: false,
            write_token: Some(RepositoryWriteToken::server(&INTERNAL_SERVER_CONTEXT)),
            repo_lock: None,
            session_pool: Default::default(),
            lazy_session: Default::default(),
        }
    }

    pub fn to_null_context(&self) -> Self {
        RepositoryContext {
            link_read: self.link_read.clone(),
            paths: self.paths.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id: RepositoryId::default(),
            root_id: RepositoryId::default(),
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::Offline),
            filter: self.filter.clone(),
            settings: self.settings.clone(),
            is_link: false,
            is_layer: false,
            write_token: None,
            repo_lock: None,
            session_pool: Default::default(),
            lazy_session: Default::default(),
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
            link_read: self.link_read.clone(),
            paths: self.paths.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id: self.id,
            root_id: self.root_id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::from_result(remote)),
            filter,
            settings: self.settings.clone(),
            is_link: self.is_link,
            is_layer: self.is_layer,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            session_pool: Default::default(),
            lazy_session: Default::default(),
            file_system: self.file_system.clone(),
        }
    }

    /// This context aimed at the repository a link mounts, keeping the working tree it is
    /// materialized into.
    ///
    /// The mounted repository holds its own tree of nodes, and a node in it is named by the
    /// state and node id a caller already holds. Every path in a working tree is spelled
    /// relative to the root this keeps, so the paths a walk carries across a mount stay the
    /// paths the filesystem, the filter and the modified-time keys answer for. A path within
    /// the mounted tree is derived from its node where one is called for, by
    /// [`State::node_path`](crate::state::State::node_path).
    ///
    /// The filter is carried over as the same handle, not rebuilt. A diff tells one view from two
    /// by pointer identity on it, so a fresh handle holding the same rules would leave every diff
    /// across the mount doing two-view work for a view that has not changed.
    pub async fn to_link_context(&self, id: RepositoryId) -> Arc<Self> {
        let remote = self.remote().await;
        let remote = if let Ok(remote) = remote {
            remote.connect_module(id).await
        } else {
            remote
        };
        let settings = self.settings.clone();
        Arc::new(RepositoryContext {
            link_read: self.link_read.clone(),
            paths: self.paths.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id,
            root_id: self.root_id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::from_result(remote)),
            filter: self.filter.clone(),
            settings,
            is_link: true,
            is_layer: false,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            session_pool: Default::default(),
            lazy_session: Default::default(),
            file_system: self.file_system.clone(),
        })
    }

    /// This context aimed at the repository a layer draws from, keeping the working tree it is
    /// materialized into, as [`Self::to_link_context`] does for a link.
    pub async fn to_layer_context(&self, id: RepositoryId) -> Self {
        let remote = self.remote().await;
        let remote = if let Ok(remote) = remote {
            remote.connect_module(id).await
        } else {
            remote
        };
        let settings = self.settings.clone();
        RepositoryContext {
            link_read: self.link_read.clone(),
            paths: self.paths.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id,
            root_id: self.root_id,
            instance_id: self.instance_id,
            remote: remote_arc(RemoteState::from_result(remote)),
            filter: self.filter.clone(),
            settings,
            is_link: false,
            is_layer: true,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            session_pool: Default::default(),
            lazy_session: Default::default(),
            file_system: self.file_system.clone(),
        }
    }

    /// This context reading the same repository through `filter`, inheriting everything else as
    /// the same handles, the connection state cell included.
    ///
    /// The write token travels, as it does through every other builder here. Writes deep in a read
    /// path are gated on it and skip themselves without one — chiefly
    /// [`state::file_modified_against_node`](crate::state::file_modified_against_node), which
    /// records the modified time of a file a hash check just established as unmodified — so a
    /// context without one leaves every later pass to pay the hash again. Sharing grants no
    /// authority the caller does not already hold: the guard is the one `self` holds, and it is
    /// released once every sibling drops.
    pub fn to_filter_context(&self, filter: Arc<Filter>) -> Self {
        RepositoryContext {
            link_read: self.link_read.clone(),
            paths: self.paths.clone(),
            immutable_store: self.immutable_store.clone(),
            mutable_store: self.mutable_store.clone(),
            id: self.id,
            root_id: self.root_id,
            instance_id: self.instance_id,
            remote: self.remote.clone(),
            filter,
            settings: self.settings.clone(),
            is_link: self.is_link,
            is_layer: self.is_layer,
            write_token: self.write_token.as_ref().map(|t| t.share()),
            repo_lock: self.repo_lock.clone(),
            session_pool: Default::default(),
            lazy_session: Default::default(),
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

    /// The storage session pool for this repository, resolved once and reused for
    /// as long as it lives.
    ///
    /// `correlation_id` is only read when the pool has to be resolved. It belongs
    /// to the command being executed and a context belongs to one command, so a
    /// cached pool is always the one this context's correlation id asked for.
    #[lore_macro::test_pub]
    pub(crate) async fn session_pool(
        &self,
        correlation_id: &str,
    ) -> Result<Arc<SessionPool>, ProtocolError> {
        if let Some(pool) = self.cached_session_pool() {
            return Ok(pool);
        }
        let remote = self.remote().await?;
        let pool = remote.session_pool(self.id, correlation_id).await?;
        *self.session_pool.write() = Arc::downgrade(&pool);
        Ok(pool)
    }

    /// The lazy storage session for this context, built once and shared. Partition-scoped,
    /// so a derived context for a link or a layer resolves its own rather than borrowing one
    /// started for another partition.
    pub(crate) fn lazy_session(
        &self,
        build: impl FnOnce() -> Arc<lore_transport::StorageSession>,
    ) -> Arc<lore_transport::StorageSession> {
        if let Some(session) = self.lazy_session.read().as_ref() {
            return session.clone();
        }
        self.lazy_session.write().get_or_insert_with(build).clone()
    }

    /// The session pool if one is already resolved and still live, without
    /// touching the remote. `None` means "resolve it", not "there is none".
    ///
    /// Private so the pool is only ever reached through [`Self::session_pool`]. A
    /// caller handed the pool directly would pick from it eagerly, and an eager
    /// session cannot be invalidated and retried — see [`crate::immutable`]'s
    /// session resolution.
    #[lore_macro::test_pub]
    fn cached_session_pool(&self) -> Option<Arc<SessionPool>> {
        self.session_pool.read().upgrade()
    }

    /// Hold `pool` as resolved, standing in for a resolution against a live remote.
    /// The caller keeps the strong reference, as the connection's session cache
    /// does.
    #[cfg(feature = "test-util")]
    pub fn set_session_pool(&self, pool: &Arc<SessionPool>) {
        *self.session_pool.write() = Arc::downgrade(pool);
    }

    /// Whether this context is known to hold no remote, answered without waiting
    /// on the state lock or driving a pending connect.
    ///
    /// [`RemoteState::Offline`] is terminal, so a `true` answer stands for the life
    /// of the context and a caller can skip building a storage session outright.
    /// Everything else answers `false`, a state lock held elsewhere included: that
    /// costs only the session the caller would have built anyway. A failed connect
    /// is not offline — a session built on one reports the failure, which is the
    /// answer that belongs to it.
    #[lore_macro::test_pub]
    pub(crate) fn is_offline(&self) -> bool {
        self.remote
            .try_read()
            .is_ok_and(|state| matches!(&*state, RemoteState::Offline))
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

/// Whether `name` is the repository's own directory, in any ASCII case.
///
/// No node carries such a name: a writer refuses it, a per-node read refuses it, and a walk
/// skips the node, so content in a revision never stands in for the control directory, on a
/// filesystem that folds case as much as on one that does not.
#[inline]
pub fn is_reserved_node_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(DOT_URC) || name.eq_ignore_ascii_case(DOT_LORE)
}

pub const SALT_URC: &[u8] = b"urc";
// We cannot easily change this as it is also used on server to create
// the mutable keys - it would lose track of existing repos in production
// So for now keep using "urc" salt when hashing mutable keys
pub const SALT_LORE: &[u8] = b"urc";
//pub const SALT_LORE: &[u8] = b"lore";

pub const VIEW_FILTER: &str = "view";
pub const LAYER: &str = "layer.toml";
pub const TEMP_FILE_EXTENSION: &str = ".~loretemp";
pub const BASE_SUFFIX: &str = "~base";
pub const THEIRS_SUFFIX: &str = "~theirs";
pub const MINE_SUFFIX: &str = "~mine";

/// The suffixes a conflicted merge names its copies of a file with, beside the file itself in
/// the working tree.
///
/// A conflict writes only the sides it has, so fewer than three may be present, and a clean
/// automerge removes the ones it wrote.
pub const MERGE_ARTIFACT_SUFFIXES: [&str; 3] = [MINE_SUFFIX, THEIRS_SUFFIX, BASE_SUFFIX];

pub fn get_dot_lore_path(path: &std::path::Path) -> Result<PathBuf, InvalidPath> {
    if let Some(mount_manager) = MountManagerState::mount_manager() {
        match mount_manager.check_for_external_lore_dir(path) {
            Ok(Some(path)) => {
                return Ok(path);
            }
            Ok(None) => {}
            Err(_) => {
                return Err(InvalidPath {
                    path: format!("{}", path.display()),
                });
            }
        }
    };
    let legacy = path.join(DOT_URC);
    Ok(if legacy.is_dir() {
        legacy
    } else {
        path.join(DOT_LORE)
    })
}

/// Splits a repository URL into its remote URL and repository name.
///
/// With `allow_no_remote`, an argument carrying no URL scheme is a repository name and
/// yields an empty remote URL, naming a repository that has no remote at all. Callers
/// that require a reachable remote — clone, delete, info, link — pass `false` so a
/// missing host is an error rather than a silently local repository.
pub fn parse_url(url: &str, allow_no_remote: bool) -> Result<(String, String), RepositoryError> {
    let url = if url.contains("://") {
        url::Url::parse(url).internal("Invalid URL")?
    } else {
        // No scheme means no host to find, so the whole argument is the name. That
        // includes a slash-separated one such as `org/project`, which `is_valid_name`
        // supports: reading the first segment as a host would both truncate the name and
        // record a remote the caller never configured. Naming a remote takes a scheme.
        if allow_no_remote {
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

/// Reads the repository config, defaulting only when it is absent.
///
/// Read synchronously — see [`util::config::load_blocking`], which carries the reasoning. A
/// config that is present but unreadable is an error rather than a default: this file holds the
/// remote URL, so defaulting past a read failure presents a repository that merely could not be
/// opened as one with no remote, and the next save writes that back.
fn load_config(config_path: impl AsRef<Path>) -> Result<RepositoryConfig, RepositoryError> {
    util::config::load_blocking(config_path)
        .forward::<RepositoryError>("Failed to load config file")
}

async fn save_config(
    config_path: impl AsRef<Path>,
    config: &RepositoryConfig,
) -> Result<(), RepositoryError> {
    util::config::save(config, config_path)
        .await
        .forward::<RepositoryError>("Failed to save config file")
}

pub fn load_repository_config_from_dot_dir(
    dot_path: &Path,
) -> Result<RepositoryConfig, RepositoryError> {
    let config_path = dot_path.join(CONFIG);

    load_config(config_path.as_path())
}

pub fn load_repository_config(path: impl AsRef<Path>) -> Result<RepositoryConfig, RepositoryError> {
    load_repository_config_from_dot_dir(&get_dot_lore_path(path.as_ref())?)
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

    // The OS flock is taken with non-blocking attempts and async retries,
    // so a contended lock never stalls a runtime thread.
    let lock = Box::pin(FSLock::acquire_directory_lock(dot_path.clone()))
        .await
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

/// Stop garbage collection for good on every store still live in the caches, so a pass in
/// flight gives up at its next packfile rather than holding shutdown open until it has
/// rewritten the rest of the group, and none follows it. For shutdown, where no further
/// call will be made; a caller that will use the store again wants
/// [`lore_storage::ImmutableStore::stop_gc`] without `terminate`.
pub async fn stop_store_gc() {
    // Collected before awaiting: a `DashMap` guard held across an await blocks every other
    // reader of the cache.
    let mut stores: Vec<Arc<dyn ImmutableStore>> = Vec::new();
    if let Some(cache) = IMMUTABLE_STORE_CACHE.get() {
        stores.extend(cache.iter().filter_map(|entry| entry.value().upgrade()));
    }
    if let Some(cache) = IN_MEMORY_IMMUTABLE_CACHE.get() {
        stores.extend(cache.iter().map(|entry| entry.value().clone()));
    }

    // Driven together so every store is asked to stop on the first poll, rather than each
    // waiting out the one before it.
    futures::future::join_all(stores.into_iter().map(|store| store.stop_gc(true))).await;
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
    create_immutable_store_at_path(
        get_shared_store_path_for_repo(config)
            .await
            .forward::<RepositoryError>("Failed to access shared store")?
            .unwrap_or_else(|| dotpath.as_ref().to_owned()),
        create_options,
        verify_write,
    )
    .await
}

pub async fn create_immutable_store_at_path(
    path: PathBuf,
    create_options: ImmutableStoreCreateOptions,
    verify_write: bool,
) -> Result<Arc<dyn ImmutableStore>, RepositoryError> {
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

    create_mutable_store_at_path(path, immutable_store).await
}

pub async fn create_mutable_store_at_path(
    path: PathBuf,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<Arc<crate::store::mutable::MutableStore>, RepositoryError> {
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
        // Pre-warm the session pool so it's ready when the command runs
        if !repository.is_zero() {
            let _ = connection.session_pool(repository, &correlation_id).await;
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

pub fn read_id_from_file(path: PathBuf) -> io::Result<RepositoryId> {
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
///
/// Instance ID recovery and instance registration are boxed. They run only for
/// an instance missing its ID file or its registration, and inline they would
/// make every command's future as large as theirs.
pub async fn load_and_connect_with_token(
    path: &Path,
    access: RepositoryAccess,
    write_token: Option<RepositoryWriteToken>,
) -> Result<Arc<RepositoryContext>, RepositoryError> {
    require_text_root(path)?;
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
            Some(RepositoryWriteToken::InMemory) => "Some(InMemory)",
        },
    );

    let dot_path = get_dot_lore_path(path)?;

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

    let filter = load_filter(path)?;

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
        let recovered = Box::pin(crate::instance::recover_instance_id(
            repository,
            mutable_store.clone(),
            immutable_store.clone(),
            &path.display().to_string(),
        ))
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

    // Load the mounted filesystem if this is a SWFS-backed instance
    let filesystem: Option<Arc<dyn FilesystemProvider + 'static>> = if config.is_swfs() {
        let mount_manager = MountManagerState::mount_manager().ok_or(RepositoryError::internal(
            "Loading a SWFS repository without using the service",
        ))?;
        Some(
            mount_manager
                .get_mount_filesystem_provider(path)
                .forward::<RepositoryError>("Unable to find mount for SWFS repository")?,
        )
    } else {
        None
    };

    // Keep the remote pending so local-only commands finish without waiting on the
    // background connect. The upgrade path above already forced resolution when needed.
    let remote_state = match (resolved_remote_for_upgrade, remote) {
        (Some(resolved), _) => RemoteState::from_result(resolved),
        (None, Ok(remote_fut)) => RemoteState::Pending(remote_fut),
        (None, Err(err)) => RemoteState::from_result(Err(err)),
    };
    let repository = RepositoryContext::new_with_state(
        Some(RepositoryPaths::new(path.to_path_buf(), dot_path.clone())),
        immutable_store,
        mutable_store,
        repository,
        instance_id,
        remote_state,
        filter,
        filesystem,
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

    repository.set_disable_cache(!(global.cache() || config.is_swfs()));
    let config_file = config.file.unwrap_or_default();
    repository.set_direct_file_write(config_file.direct_write.unwrap_or_default());

    if global.local() {
        repository.set_disable_upload(true);
    }

    let repository = Arc::new(repository);

    // Instance registration and lazy anchor migrations only run in write-mode
    // contexts — read-only commands shouldn't mutate persistent state during
    // load_and_connect. A read-only invocation that hits a repository needing
    // registration/migration simply defers the work to the next write command.
    if repository.try_write_token().is_some() {
        register_instance_and_migrate_anchors(&repository, instance_id, path, &dot_path).await?;
    }

    Ok(repository)
}

/// The write-mode part of [`load_and_connect_with_token`]: registers the
/// instance if needed and moves file-based anchors to the mutable store.
///
/// A function of its own because its locals live across several awaits: kept
/// in [`load_and_connect_with_token`] they would take space in its future while
/// the stores are created as well.
async fn register_instance_and_migrate_anchors(
    repository: &Arc<RepositoryContext>,
    instance_id: crate::instance::InstanceId,
    path: &Path,
    dot_path: &Path,
) -> Result<(), RepositoryError> {
    // Register instance if not already present in the mutable store.
    // This covers both newly generated IDs and pre-existing instances
    // upgrading from a version before instance registration was added.
    // Registration also retires any registration another instance left
    // at this path, so a re-created checkout is listed once.
    let (instance_key, instance_key_type) =
        crate::instance::instance_key(repository.salt(), instance_id);
    let needs_registration = repository
        .read_mutable_store()
        .load(repository.id, instance_key, instance_key_type)
        .await
        .map_or(true, |h| h.is_zero());
    if needs_registration
        && let Err(err) = Box::pin(crate::instance::register_instance(
            repository,
            instance_id,
            &path.display().to_string(),
        ))
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
        crate::instance::store_current_anchor_branch(repository, branch)
            .await
            .forward::<RepositoryError>("Failed to serialize repository anchor")?;
        crate::instance::store_current_anchor(repository, revision)
            .await
            .forward::<RepositoryError>("Failed to serialize repository anchor")?;
        migrated_current = true;
    }
    if staged_anchor_path.exists() {
        match crate::anchor::deserialize_migrate_old(&staged_anchor_path).await {
            Ok((revision, _branch)) => {
                match crate::instance::store_staged_anchor(repository, revision).await {
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
        if let Err(err) = lore_io::IoDriver::global()
            .remove_file(&current_anchor_path)
            .await
        {
            lore_warn!("Failed to remove old current anchor file: {err}");
        }
        lore_debug!("Migrated file-based current anchor to mutable store");
    }
    if migrated_staged {
        if let Err(err) = lore_io::IoDriver::global()
            .remove_file(&staged_anchor_path)
            .await
        {
            lore_warn!("Failed to remove old staged anchor file: {err}");
        }
        lore_debug!("Migrated file-based staged anchor to mutable store");
    }

    Ok(())
}

pub const MAX_NAME_LEN: usize = 1000;
pub const MAX_DESCRIPTION_LEN: usize = 65536;

/// A name is a `/`-separated path of segments, each holding only ASCII
/// alphanumerics, `-`, `_` and `.`.
///
/// Empty and dot-leading segments are rejected because names round-trip through
/// URLs (see [`parse_url`]), and URL parsing rewrites them: `host/.` and
/// `host/..` lose the name entirely, `host/org/../other` silently resolves to
/// `other`, and a trailing `/` is trimmed. The server stores such names fine, so
/// without this they produce repositories no client can address, or names that
/// resolve to a different repository than the one written. Dot-leading segments
/// also carry filesystem meaning for tooling that maps a name onto a directory.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .find(|c: char| {
                !c.is_ascii_alphanumeric() && (c != '/') && (c != '-') && (c != '_') && (c != '.')
            })
            .is_none()
        && name
            .split('/')
            .all(|segment| !segment.is_empty() && !segment.starts_with('.'))
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
    require_text_root(path)?;
    let instance_id = InstanceId::generate();

    let dotpath;
    let filesystem_provider: Option<Arc<dyn FilesystemProvider + 'static>>;
    if config.is_swfs() {
        let mount_manager = MountManagerState::mount_manager().ok_or(RepositoryError::internal(
            "Attempting to create an SWFS instance outside the service",
        ))?;
        dotpath = mount_manager
            .create_mount(path, &config, repository, instance_id)
            .await
            .forward::<RepositoryError>("Failed to create mount for SWFS instance")?;
        filesystem_provider = Some(
            mount_manager
                .get_mount_filesystem_provider(path)
                .forward::<RepositoryError>("Failed to get filesystem provider for fresh mount")?,
        );
    } else {
        dotpath = path.join(DOT_LORE);
        filesystem_provider = None;
    };
    let idpath = dotpath.join(ID);

    /*if dotpath.exists() {
        return Err(RepositoryError::from(RepositoryAlreadyExists {
            path: path.display().to_string(),
        }));
    }*/

    let dotpath_display = dotpath.display().to_string();
    lore_io::IoDriver::global()
        .create_dir_all(dotpath.as_path())
        .await
        .internal_with(|| {
            format!("Failed to create repository, unable to create directory {dotpath_display}")
        })?;

    let idpath_display = idpath.display().to_string();
    lore_io::IoDriver::global()
        .write_file_bytes(
            idpath.as_path(),
            bytes::Bytes::copy_from_slice(repository.data()),
            false,
        )
        .await
        .internal_with(|| {
            format!(
                "Failed to write local repository state in created tracking directory {idpath_display}"
            )
        })?;

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
        RepositoryContext::new(RepositoryContextCreationArgs {
            paths: Some(RepositoryPaths::new(path.to_path_buf(), dotpath.clone())),
            immutable_store,
            mutable_store,
            id: repository,
            instance_id,
            remote: Err(ProtocolError::from(NoRemote)),
            filter: Arc::default(),
            filesystem_provider,
        })
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
    // we are on, even though there are no revisions yet (zero revision).
    crate::instance::store_current_anchor_branch(&repository, default_branch)
        .await
        .forward::<RepositoryError>("Failed to serialize repository anchor")?;

    // Serialize config
    let config_path = dotpath.join(CONFIG);
    save_config(config_path, &config).await?;

    repository.flush(true).await?;

    Ok(repository)
}

/// Loads the ignore and view filters for the repository rooted at `root_path`.
///
/// A filter file that cannot be understood fails the load rather than yielding
/// an empty filter: an empty one excludes nothing, so the caller would go on to
/// walk and stage everything the file meant to keep out.
pub fn load_filter(root_path: &Path) -> Result<Arc<filter::Filter>, RepositoryError> {
    let mut ignore_path = root_path.join(DOT_LOREIGNORE);

    // Both formats use .loreignore as the primary ignore file; fall back to
    // legacy .urcignore whenever .loreignore is not present.
    if !ignore_path.exists() {
        let fallback = root_path.join(DOT_URCIGNORE);
        if fallback.exists() {
            ignore_path = fallback;
        }
    }

    let view_path = get_dot_lore_path(root_path)?.join(VIEW_FILTER);
    let filter = filter::load(&ignore_path, &view_path)
        .forward::<RepositoryError>("Failed to load repository filter")?;
    Ok(Arc::new(filter))
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

    // `branch::create` has just seeded the pointer with the branch point; move it
    // on to the tip the remote reports.
    let created_at = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();
    branch::store_latest(
        repository.clone(),
        branch,
        created_at,
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

    let (branch_latest_local, branch_latest_remote, branch_location, branch_signature) =
        branch_switch_target(
            &repository,
            token,
            branch.id,
            branch_name,
            branch_point,
            &options,
        )
        .await?;

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
        sync::sync(repository.clone(), token, sync_options)
            .await
            .forward::<RepositoryError>("Failed to synchronize state during branch switch")?;
    }

    if !global.dry_run() {
        // A switch republishes the branch's own local tip; the sync above may have
        // moved the pointer, so compare against what it holds now.
        let stored_latest = branch::load_latest(repository.clone(), branch.id)
            .await
            .unwrap_or_default();
        branch::store_latest(
            repository.clone(),
            branch.id,
            stored_latest,
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
            state::rebase_staged_anchor(repository.clone(), branch_signature, false)
                .await
                .forward::<RepositoryError>("Failed to rebase staged anchor")?;
        }

        if branch_location == LoreBranchLocation::Remote {
            branch::store_last_sync(repository.clone(), branch.id, branch_latest_local).await;
        }

        // Restore the name-to-id mapping for the branch to ensure it shows up in the local branch list.
        let mapped = branch::load_name_to_id_local(repository.clone(), branch_name)
            .await
            .unwrap_or_default();
        if mapped != Context::default() && mapped != branch.id && !global.force() {
            return Err(RepositoryError::internal(
                "Given branch's name is already used by another branch",
            ));
        }
        if mapped == Context::default() || global.force() {
            branch::store_name_to_id(repository.clone(), branch.id, &branch_name)
                .await
                .forward::<RepositoryError>("restoring name-to-id mapping")?;
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

/// The revision a switch to `branch_id` lands on, as `(latest local, latest remote, location,
/// signature)`: the given signature, or the local or remote latest, creating the local branch
/// where only the remote holds it.
///
/// A function of its own because its locals live across several awaits: kept in
/// [`branch_switch`] they would take space in its future while the layers switch as well.
async fn branch_switch_target(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch_id: BranchId,
    branch_name: &str,
    branch_point: Hash,
    options: &BranchSwitchOptions,
) -> Result<(Hash, Hash, LoreBranchLocation, Hash), RepositoryError> {
    let context = execution_context();
    let global = context.globals();

    let signature = if let Some(revision) = options.signature.as_ref() {
        let resolved =
            revision::resolve_in_branch(repository.clone(), revision, global.search_location())
                .await
                .forward::<RepositoryError>("Invalid revision")?;
        let revision = resolved.revision;

        let state = state::State::deserialize(repository.clone(), revision)
            .await
            .forward::<RepositoryError>("Invalid revision")?;
        if state.branch(repository.clone()).await != branch_id && revision != branch_point {
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

    let local_head = branch::load_latest(repository.clone(), branch_id)
        .await
        .ok();

    let remote_branch = if options.local {
        lore_debug!("Using local latest revision");
        None
    } else {
        match repository.remote().await {
            Ok(remote) => {
                lore_debug!("Loading remote latest revision");
                match branch::load_remote(remote, repository.id, branch_id).await {
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
                    lore_debug!("Remote unavailable, switching using local branch state: {err}");
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
                            branch_id,
                            remote_head,
                            false,
                            None,
                            |state, _metadata| {
                                if state.revision() == local_head
                                    || state.parent_other() == local_head
                                {
                                    find::FindMatchResult::Match
                                } else if state.revision_number() < local_state.revision_number() {
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
                        lore_debug!("Remote and local revision are equal, treat as remote sync");
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
                lore_debug!("Failed to load remote latest revision state, sync to local latest");
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
                branch_id,
                branch_name,
                latest_local,
                latest_remote,
                signature,
                location,
            ),
        })
        .send();

        Ok((latest_local, latest_remote, location, signature))
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
                branch_id,
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
            branch_id,
            remote_status.latest,
            remote_status.metadata,
            global.dry_run(),
        )
        .await?;

        Ok((
            remote_status.latest,
            remote_status.latest,
            LoreBranchLocation::Remote,
            signature,
        ))
    }
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
    let layers = layer::list_with_context(repository.clone())
        .await
        .forward::<RepositoryError>("Failed to switch branch in layer")?;

    if layers.is_empty() {
        return Ok(());
    }

    let context = execution_context();
    let global = context.globals();

    let mut layer_updates: Vec<(RepositoryId, String, Hash)> = Vec::new();

    for (layer, layer_repository) in layers {
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

        // Check if branch already exists in layer repo
        let branch_exists = branch::exist_local(layer_repository.clone(), branch_id).await;

        let layer_current = if branch_exists {
            None
        } else {
            let current_revision =
                state::State::deserialize(layer_repository.clone(), layer.current)
                    .await
                    .forward::<RepositoryError>("Failed to deserialize repository state")?;
            let current_branch = current_revision.branch(layer_repository.clone()).await;
            Some((current_revision, current_branch))
        };

        // Skip only the branch creation, not the layer: a layer already on this
        // branch can still be behind the branch latest and needs resolving.
        if let Some((current_revision, current_branch)) = layer_current
            && current_branch != branch_id
        {
            // Create branch in layer repo - mirrors layer_branch_create pattern
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
                layer_repository.clone(),
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

    let dot_path = get_dot_lore_path(&path)?;
    let id_path = dot_path.join(ID);

    Ok(read_id_from_file(id_path).internal("Repository not found")?)
}

pub fn repository_remote(repository_path: impl AsRef<str>) -> Result<String, RepositoryError> {
    let Ok(path) = util::path::make_absolute(repository_path) else {
        return Err(RepositoryError::internal("Invalid repository path"));
    };

    let dot_path = get_dot_lore_path(&path)?;
    let config_path = dot_path.join(CONFIG);

    let Ok(config) = load_config(config_path) else {
        return Err(RepositoryError::internal("Invalid repository path"));
    };

    Ok(config.remote_url.unwrap_or_default())
}

pub async fn gc(repository: Arc<RepositoryContext>) -> Result<(), RepositoryError> {
    let dot_path = repository.dot_dir_path()?;
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
