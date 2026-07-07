// SPDX-FileCopyrightText: Nicolas Sauzede <nicolas.sauzede@gmail.com>
// SPDX-License-Identifier: MIT
//! `lore-drive` — Axum/Tokio REST backend that exposes a lore workspace as a
//! browsable file/folder tree over HTTP, with workspace-mediated writes.
//!
//! # Modes
//!
//! - **Drive mode (default)** — "dumb cloud drive" / USB-stick semantics:
//!   mutations update the working directory and the single *staged* revision
//!   (`lore::file::stage`/`stage_move`), **never committing**. There is no
//!   history: every mutation replaces the one staged snapshot in place.
//!   Uploaded content is pushed into the CAS by lore-drive itself
//!   (`lore_storage_put_file`), so files still have node ids, b3 hashes and
//!   populate the mutable/immutable stores — deduplicated: uploading the same
//!   path/content N times keeps exactly one stored copy.
//! - **Versioned mode (`--versioned`)** — every successful mutating request
//!   produces exactly one commit on the active branch (previous behavior).
//!
//! # Endpoints
//!
//! Read:
//! - `GET /api/v1/info`                  — workspace metadata (+ serving mode)
//! - `GET /api/v1/tree?node_id=<u64>`    — directory listing
//! - `GET /api/v1/node/{node_id}`        — single-node record + full path
//! - `GET /api/v1/download/{node_id}`    — file bytes (CAS, workdir fallback) or folder ZIP
//!
//! Write (filesystem change → stage[/commit]):
//! - `POST   /api/v1/mkdir`                              — create directory
//! - `POST   /api/v1/upload?parent_id=&overwrite=`       — multipart file/folder upload
//! - `PATCH  /api/v1/node/{node_id}`                     — rename / move
//! - `DELETE /api/v1/node/{node_id}`                     — delete subtree
//!
//! See `REST_API.md` for the authoritative JSON shapes and semantics.
//!
//! # Usage
//!
//! Run inside a lore workspace (same working directory where you would invoke
//! the `lore` CLI):
//!
//! ```sh
//! lore-drive                 # drive mode, listen on 0.0.0.0:8080
//! lore-drive --port 9090     # custom port
//! lore-drive --versioned     # one commit per mutation (previous behavior)
//! ```

use std::collections::HashMap;
use std::env;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Extension;
use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Multipart;
use axum::extract::Path;
use axum::extract::Query;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use clap::Parser;
use lore::revision_tree::close::LoreRevisionTreeCloseArgs;
use lore::revision_tree::close::close as tree_close;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
use lore::revision_tree::list_children::list_children;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
use lore::revision_tree::node_info::node_info;
use lore::revision_tree::node_path::LoreRevisionTreeNodePathArgs;
use lore::revision_tree::node_path::node_path;
use lore::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs;
use lore::revision_tree::resolve_path::resolve_path;
use lore::storage::get_file::LoreStorageGetFileArgs;
use lore::storage::get_file::LoreStorageGetFileItem;
use lore::storage::get_file::get_file;
use lore::storage::handle::LoreStore;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::open as storage_open;
use lore::storage::put_file::LoreStoragePutFileArgs;
use lore::storage::put_file::LoreStoragePutFileItem;
use lore::storage::put_file::put_file;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeChildEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::load_and_connect;
use serde::Deserialize;
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tracing::info;
use tracing::warn;

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "lore-drive", about = "Lore workspace REST backend")]
struct Cli {
    /// TCP port to listen on
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Commit every mutation to the active branch (default: drive mode —
    /// mutations only update the single staged revision, no history)
    #[arg(long, default_value_t = false)]
    versioned: bool,
}

// ─── App state ───────────────────────────────────────────────────────────────

/// The parts of the state that change after every successful mutation.
#[derive(Copy, Clone)]
struct TreeState {
    /// Loaded revision-tree handle for `revision`.
    tree: LoreRevisionTree,
    /// The revision hash the handle was loaded from (staged revision in drive
    /// mode once one exists; committed branch revision otherwise).
    revision: Hash,
}

/// Shared state injected into every handler via `axum::Extension`.
struct AppState {
    /// Open repository context (kept alive so the underlying stores remain open).
    repository: Arc<RepositoryContext>,
    /// Open content-addressed storage handle (for CAS reads/writes and reloads).
    store: LoreStore,
    /// Revision-dependent state — swapped after each successful mutation.
    tree_state: tokio::sync::RwLock<TreeState>,
    /// Serializes all mutating requests (one stage[/commit] at a time).
    write_gate: tokio::sync::Mutex<()>,
    /// `--versioned`: commit each mutation instead of stage-only drive mode.
    versioned: bool,
    /// Drive mode: authoritative CAS addresses for file nodes whose staged
    /// record still carries a zero content hash (content hashing is a commit
    /// step, which drive mode never runs).  Keyed by node id, valid for the
    /// currently served revision only — cleared on every tree swap.
    addr_cache: tokio::sync::RwLock<HashMap<u32, Address>>,
    /// Repository identity.
    repository_id: RepositoryId,
    /// Identity of the active branch.
    branch_id: BranchId,
    /// Human-readable name of the active branch (may be empty for detached).
    branch_name: String,
    /// Absolute path of the workspace root.
    workdir: PathBuf,
}

impl AppState {
    async fn tree(&self) -> LoreRevisionTree {
        self.tree_state.read().await.tree
    }
    async fn revision(&self) -> Hash {
        self.tree_state.read().await.revision
    }
    fn mode_str(&self) -> &'static str {
        if self.versioned { "versioned" } else { "drive" }
    }
}

// ─── JSON response types ─────────────────────────────────────────────────────

/// `GET /api/v1/info` response.
#[derive(Serialize)]
struct InfoResponse {
    repository_id: String,
    branch_id: String,
    branch_name: String,
    revision: String,
    workdir: String,
    /// `"drive"` (stage-only, no history) or `"versioned"` (commit per mutation).
    mode: String,
}

/// One child entry inside `GET /api/v1/tree` response.
#[derive(Serialize)]
struct ChildEntry {
    node_id: u64,
    name: String,
    kind: String,
    mode: u16,
    size: u64,
    /// `None` for pure directories; `"<hash>-<context>"` for files and links.
    address: Option<String>,
}

/// `GET /api/v1/tree` response.
#[derive(Serialize)]
struct TreeResponse {
    repository_id: String,
    revision: String,
    node_id: u64,
    children: Vec<ChildEntry>,
}

/// `GET /api/v1/node/{node_id}` response (also embedded in PATCH response).
#[derive(Serialize)]
struct NodeResponse {
    node_id: u64,
    parent_id: u64,
    name: String,
    kind: String,
    mode: u16,
    size: u64,
    address: Option<String>,
    path: String,
}

/// `POST /api/v1/mkdir` response.
#[derive(Serialize)]
struct MkdirResponse {
    node_id: Option<u64>,
    path: String,
    revision: String,
}

/// One created file inside `POST /api/v1/upload` response.
#[derive(Serialize)]
struct UploadedFile {
    name: String,
    path: String,
    node_id: Option<u64>,
    size: u64,
    address: Option<String>,
}

/// `POST /api/v1/upload` response.
#[derive(Serialize)]
struct UploadResponse {
    revision: String,
    files: Vec<UploadedFile>,
}

/// `PATCH /api/v1/node/{id}` response.
#[derive(Serialize)]
struct PatchResponse {
    #[serde(flatten)]
    node: NodeResponse,
    revision: String,
}

/// `DELETE /api/v1/node/{id}` response.
#[derive(Serialize)]
struct DeleteResponse {
    revision: String,
}

/// Uniform error body for 4xx / 5xx responses.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicts: Option<Vec<String>>,
}

type ApiError = (StatusCode, Json<ErrorBody>);

fn err_resp(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (
        status,
        Json(ErrorBody {
            error: msg.into(),
            conflicts: None,
        }),
    )
}

fn conflict_resp(msg: impl Into<String>, conflicts: Vec<String>) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(ErrorBody {
            error: msg.into(),
            conflicts: Some(conflicts),
        }),
    )
}

// ─── Helpers: node kinds, names, paths ───────────────────────────────────────

fn kind_str(kind: u32) -> &'static str {
    if kind == LoreNodeType::File as u32 {
        "file"
    } else if kind == LoreNodeType::Link as u32 {
        "link"
    } else {
        "directory"
    }
}

fn is_file(kind: u32) -> bool {
    kind == LoreNodeType::File as u32
}

fn is_dir(kind: u32) -> bool {
    kind == LoreNodeType::Directory as u32
}

fn is_link(kind: u32) -> bool {
    kind == LoreNodeType::Link as u32
}

/// Returns `None` for directories, `Some(address.to_string())` for files/links.
fn address_opt(kind: u32, address: Address) -> Option<String> {
    if is_dir(kind) { None } else { Some(address.to_string()) }
}

/// Validate a single path component for mkdir / rename / upload segments.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Validate and normalize a multipart `filename` into safe relative segments.
fn sanitize_rel_path(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('/') {
        return None;
    }
    let segments: Vec<String> = raw
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.is_empty() || !segments.iter().all(|s| valid_name(s)) {
        return None;
    }
    Some(segments)
}

/// Join a workspace-absolute virtual path ("/a/b") with a name → "/a/b/name".
fn join_virtual(parent: &str, name: &str) -> String {
    if parent == "/" { format!("/{name}") } else { format!("{parent}/{name}") }
}

/// Workspace-relative form (no leading slash) of a virtual path; "" for root.
fn rel_of(virtual_path: &str) -> &str {
    virtual_path.trim_start_matches('/')
}

fn parse_node_id(raw: u64) -> Result<u32, ApiError> {
    if raw > u32::MAX as u64 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "node_id out of u32 range"));
    }
    let id = raw as u32;
    if id == INVALID_NODE {
        return Err(err_resp(StatusCode::BAD_REQUEST, "node_id is the invalid sentinel"));
    }
    Ok(id)
}

// ─── Helpers: lore verb wrappers (callback → value) ──────────────────────────

/// Fetch the full info record of a node. `Err` carries a ready API error.
async fn fetch_node_info(
    tree: LoreRevisionTree,
    node_id: u32,
) -> Result<LoreRevisionTreeNodeInfoEventData, ApiError> {
    let sink: Arc<Mutex<Option<LoreRevisionTreeNodeInfoEventData>>> = Arc::new(Mutex::new(None));
    let cb_sink = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeNodeInfo(data) = event {
            *cb_sink.lock().unwrap() = Some(data.clone());
        }
    }));

    let status = node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs { id: 0, handle: tree, node_id },
        callback,
    )
    .await;

    let data = sink.lock().unwrap().clone();
    match data {
        Some(d) if status == 0 && d.error_code == LoreErrorCode::None => Ok(d),
        Some(d) if d.error_code == LoreErrorCode::InvalidArguments => {
            Err(err_resp(StatusCode::NOT_FOUND, "node not found"))
        }
        _ => Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, "node_info failed")),
    }
}

/// Reconstruct the "/"-prefixed path of a node ("/": root itself).
async fn fetch_node_path(tree: LoreRevisionTree, node_id: u32) -> Result<String, ApiError> {
    let sink: Arc<Mutex<Option<(String, LoreErrorCode)>>> = Arc::new(Mutex::new(None));
    let cb_sink = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeNodePath(data) = event {
            *cb_sink.lock().unwrap() = Some((data.path.as_str().to_owned(), data.error_code));
        }
    }));

    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs { id: 0, handle: tree, node_id },
        callback,
    )
    .await;

    let data = sink.lock().unwrap().clone();
    match data {
        Some((raw, LoreErrorCode::None)) if status == 0 => {
            Ok(if raw.is_empty() { "/".to_owned() } else { format!("/{raw}") })
        }
        _ => Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, "node_path failed")),
    }
}

/// List the direct children of a directory node.
async fn fetch_children(
    tree: LoreRevisionTree,
    parent_node_id: u32,
) -> Result<(RepositoryId, Hash, Vec<LoreRevisionTreeChildEventData>), ApiError> {
    struct ListSink {
        repository_id: RepositoryId,
        revision: Hash,
        begin_error: LoreErrorCode,
        children: Vec<LoreRevisionTreeChildEventData>,
    }

    let sink: Arc<Mutex<ListSink>> = Arc::new(Mutex::new(ListSink {
        repository_id: RepositoryId::default(),
        revision: Hash::default(),
        begin_error: LoreErrorCode::None,
        children: Vec::new(),
    }));

    let cb_sink = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
        LoreEvent::RevisionTreeListChildrenBegin(data) => {
            let mut s = cb_sink.lock().unwrap();
            s.repository_id = data.repository;
            s.revision = data.revision;
            s.begin_error = data.error_code;
        }
        LoreEvent::RevisionTreeChild(data) => {
            cb_sink.lock().unwrap().children.push(data.clone());
        }
        _ => {}
    }));

    let status = list_children(
        LoreGlobalArgs::default(),
        LoreRevisionTreeListChildrenArgs { id: 0, handle: tree, parent_node_id },
        callback,
    )
    .await;

    let s = sink.lock().unwrap();
    if status != 0 || s.begin_error != LoreErrorCode::None {
        return Err(err_resp(
            StatusCode::BAD_REQUEST,
            "node_id is not a valid directory node",
        ));
    }
    Ok((s.repository_id, s.revision, s.children.clone()))
}

/// Resolve a workspace-relative path (no leading '/') to a node id.
/// `None` when the path does not exist in the served tree.
async fn try_resolve_path(tree: LoreRevisionTree, rel: &str) -> Option<u32> {
    let sink: Arc<Mutex<Option<(u32, LoreErrorCode)>>> = Arc::new(Mutex::new(None));
    let cb_sink = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeResolvePathComplete(data) = event {
            *cb_sink.lock().unwrap() = Some((data.node_id, data.error_code));
        }
    }));

    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 0,
            handle: tree,
            path: LoreString::from(rel),
        },
        callback,
    )
    .await;

    let data = *sink.lock().unwrap();
    match data {
        Some((node_id, LoreErrorCode::None)) if status == 0 && node_id != INVALID_NODE => {
            Some(node_id)
        }
        _ => None,
    }
}

/// Fetch one file's content from the CAS into `dest` (via `lore_storage_get_file`).
async fn cas_fetch_to_file(
    store: LoreStore,
    partition: RepositoryId,
    address: Address,
    dest: &FsPath,
) -> anyhow::Result<()> {
    let sink: Arc<Mutex<Option<LoreErrorCode>>> = Arc::new(Mutex::new(None));
    let cb_sink = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::StorageGetItemComplete(data) = event {
            *cb_sink.lock().unwrap() = Some(data.error_code);
        }
    }));

    let item = LoreStorageGetFileItem {
        id: 0,
        partition,
        address,
        path: LoreString::from(dest.to_string_lossy().as_ref()),
        local_cache: 0,
    };

    let status = get_file(
        LoreGlobalArgs::default(),
        LoreStorageGetFileArgs {
            handle: store,
            items: LoreArray::from_vec(vec![item]),
        },
        callback,
    )
    .await;

    let code = *sink.lock().unwrap();
    match code {
        Some(LoreErrorCode::None) if status == 0 => Ok(()),
        Some(code) => anyhow::bail!("get_file failed: {code:?}"),
        None => anyhow::bail!("get_file emitted no completion (status {status})"),
    }
}

/// Store one workspace file's content into the CAS at `(partition, context)`
/// via `lore_storage_put_file`. Returns the computed content address.
/// Deduplicated by construction: identical bytes hash to the same address.
async fn cas_put_file(
    store: LoreStore,
    partition: RepositoryId,
    context: Context,
    src: &FsPath,
) -> anyhow::Result<Address> {
    let sink: Arc<Mutex<Option<(Address, LoreErrorCode)>>> = Arc::new(Mutex::new(None));
    let cb_sink = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::StoragePutItemComplete(data) = event {
            *cb_sink.lock().unwrap() = Some((data.address, data.error_code));
        }
    }));

    let item = LoreStoragePutFileItem {
        id: 0,
        partition,
        context,
        path: LoreString::from(src.to_string_lossy().as_ref()),
        remote_write: 0,
        local_cache: 0,
        fixed_size_chunk: 0,
    };

    let status = put_file(
        LoreGlobalArgs::default(),
        LoreStoragePutFileArgs {
            handle: store,
            items: LoreArray::from_vec(vec![item]),
        },
        callback,
    )
    .await;

    let data = *sink.lock().unwrap();
    match data {
        Some((address, LoreErrorCode::None)) if status == 0 => Ok(address),
        Some((_, code)) => anyhow::bail!("put_file failed: {code:?}"),
        None => anyhow::bail!("put_file emitted no completion (status {status})"),
    }
}

/// The address to expose for a node — directories get `None`.
///
/// Drive-mode subtlety: **staged file nodes carry a zero content hash** —
/// hashing and immutable-store writes are performed by the *commit* step
/// (`lore-revision/src/commit.rs`, `write_from_file_with_tracker` + directory
/// rehash), which drive mode never runs.  For such nodes this helper
/// materializes the authoritative address by storing the workdir bytes into
/// the CAS at the node's `file_id` context (`cas_put_file` — idempotent,
/// dedup by content hash) and caches the result for the current change-tag.
/// This both fixes the displayed hash (1-to-1 with the CAS) and guarantees
/// the content is actually in the store, even for files staged by an
/// external `lore` CLI.
///
/// Zero-*size* files legitimately keep the zero hash (lore's empty-content
/// convention), so they are exposed as-is without a put.
async fn effective_address(
    state: &AppState,
    tree: LoreRevisionTree,
    node_id: u32,
    kind: u32,
    size: u64,
    address: Address,
) -> Option<Address> {
    if is_dir(kind) {
        return None;
    }
    let needs_materialization =
        is_file(kind) && size > 0 && address.hash == Hash::default();
    if !needs_materialization {
        return Some(address);
    }
    if let Some(cached) = state.addr_cache.read().await.get(&node_id).copied() {
        return Some(cached);
    }
    let virtual_path = match fetch_node_path(tree, node_id).await {
        Ok(p) => p,
        Err(_) => return Some(address),
    };
    let src = state.workdir.join(rel_of(&virtual_path));
    match cas_put_file(state.store, state.repository_id, address.context, &src).await {
        Ok(stored) => {
            state.addr_cache.write().await.insert(node_id, stored);
            Some(stored)
        }
        Err(e) => {
            warn!("address materialization failed for {virtual_path}: {e}");
            Some(address)
        }
    }
}

/// String form of [`effective_address`] for JSON responses.
async fn effective_address_str(
    state: &AppState,
    tree: LoreRevisionTree,
    node_id: u32,
    kind: u32,
    size: u64,
    address: Address,
) -> Option<String> {
    effective_address(state, tree, node_id, kind, size, address)
        .await
        .map(|a| a.to_string())
}

/// Run `lore::file::stage` on workspace-relative paths. Returns the resulting
/// staged revision hash from the `FileStageRevision` event (unchanged hash ⇒
/// nothing was staged).
async fn stage_paths(paths: Vec<String>) -> anyhow::Result<Hash> {
    let (status, staged, errors) = run_stage_like(|callback| async move {
        lore::file::stage(
            LoreGlobalArgs::default(),
            lore::file::LoreFileStageArgs {
                paths: LoreArray::from_vec(
                    paths.iter().map(|p| LoreString::from(p.as_str())).collect(),
                ),
                case_change: 0,
                scan: 1,
            },
            callback,
        )
        .await
    })
    .await;
    match staged {
        Some(rev) if status == 0 => Ok(rev),
        _ if status == 0 => anyhow::bail!("stage emitted no staged revision"),
        _ => anyhow::bail!("stage failed (status {status}): {}", errors.join("; ")),
    }
}

/// Run `lore::file::stage_move` (from → to, workspace-relative paths).
/// Returns the resulting staged revision hash.
async fn stage_move_path(from: &str, to: &str) -> anyhow::Result<Hash> {
    let (status, staged, errors) = run_stage_like(|callback| async move {
        lore::file::stage_move(
            LoreGlobalArgs::default(),
            lore::file::LoreFileStageMoveArgs {
                from_path: LoreString::from(from),
                to_path: LoreString::from(to),
            },
            callback,
        )
        .await
    })
    .await;
    match staged {
        Some(rev) if status == 0 => Ok(rev),
        _ if status == 0 => anyhow::bail!("stage_move emitted no staged revision"),
        _ => anyhow::bail!("stage_move failed (status {status}): {}", errors.join("; ")),
    }
}

/// Run a stage-like verb, collecting the `FileStageRevision` hash and any
/// `LoreEvent::Error` messages next to the status.
async fn run_stage_like<F, Fut>(f: F) -> (i32, Option<Hash>, Vec<String>)
where
    F: FnOnce(LoreEventCallback) -> Fut,
    Fut: std::future::Future<Output = i32>,
{
    let staged: Arc<Mutex<Option<Hash>>> = Arc::new(Mutex::new(None));
    let staged_cb = staged.clone();
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let err_cb = errors.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
        LoreEvent::FileStageRevision(data) => {
            *staged_cb.lock().unwrap() = Some(data.revision);
        }
        LoreEvent::Error(data) => {
            err_cb.lock().unwrap().push(data.error_inner.as_str().to_owned());
        }
        _ => {}
    }));
    let status = f(callback).await;
    let staged = *staged.lock().unwrap();
    let errors = errors.lock().unwrap().clone();
    (status, staged, errors)
}

/// Commit staged changes (versioned mode only).
///
/// Returns `Ok(Some(hash))` for an effective commit and `Ok(None)` for the
/// *nothing staged* outcome — single-version semantics treat a mutation that
/// changes nothing (e.g. re-uploading identical content) as a success no-op.
async fn commit_staged(message: &str) -> anyhow::Result<Option<Hash>> {
    let revision_sink: Arc<Mutex<Option<Hash>>> = Arc::new(Mutex::new(None));
    let rev_cb = revision_sink.clone();
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let err_cb = errors.clone();

    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
        LoreEvent::RevisionCommitRevision(data) => {
            *rev_cb.lock().unwrap() = Some(data.revision);
        }
        LoreEvent::Error(data) => {
            err_cb.lock().unwrap().push(data.error_inner.as_str().to_owned());
        }
        _ => {}
    }));

    let message = message.to_owned();
    let status = lore::revision::commit(
        LoreGlobalArgs::default(),
        lore::revision::LoreRevisionCommitArgs {
            message: LoreString::from(message.as_str()),
            ..Default::default()
        },
        callback,
    )
    .await;

    let revision = *revision_sink.lock().unwrap();
    let errors = errors.lock().unwrap().clone();
    match revision {
        Some(revision) if status == 0 => Ok(Some(revision)),
        _ if is_nothing_staged(&errors) => Ok(None),
        _ => anyhow::bail!(
            "commit failed (status {status}): {}",
            if errors.is_empty() { "unknown error".to_owned() } else { errors.join("; ") }
        ),
    }
}

/// Detect the `NothingStaged` commit outcome from collected error messages.
/// Verify the exact wording during the smoke-test task and tighten this
/// match if a structured error code turns out to be observable here.
fn is_nothing_staged(errors: &[String]) -> bool {
    errors.iter().any(|e| {
        let e = e.to_ascii_lowercase();
        e.contains("nothingstaged") || (e.contains("nothing") && e.contains("staged"))
    })
}

/// Run a future inside a fresh `LORE_CONTEXT` scope. Needed for raw
/// `lore_revision` functions (like anchor loads) that are not verbs.
async fn with_lore_ctx<T, Fut>(fut: Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let ctx: Arc<dyn std::any::Any + Send + Sync> = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::no_dispatch(),
    ));
    LORE_CONTEXT.scope(ctx, fut).await
}

/// The revision this instance should serve. **Must run inside a LORE_CONTEXT
/// scope.**  Drive mode prefers the staged revision when one exists; versioned
/// mode (and a workspace with nothing staged) serves the committed anchor.
async fn served_revision(
    repository: &Arc<RepositoryContext>,
    versioned: bool,
) -> anyhow::Result<Hash> {
    if !versioned
        && let Ok(Some(staged)) = lore_revision::instance::load_staged_revision(repository).await
    {
        return Ok(staged);
    }
    let (revision, _branch) = lore_revision::instance::load_current_anchor(repository).await?;
    Ok(revision)
}

/// Mutation epilogue: in versioned mode, commit the staged changes (a
/// *nothing staged* outcome is a success no-op — the change-tag simply stays
/// the same); then reload the served revision + a fresh tree handle and swap
/// it into `state.tree_state`, closing the previous handle.
async fn finalize_mutation(state: &AppState, commit_msg: &str) -> anyhow::Result<Hash> {
    if state.versioned {
        let _maybe_new = commit_staged(commit_msg).await?; // None ⇒ no-op
    }
    refresh_tree(state).await
}

/// Reload the served revision and swap in a fresh tree handle.
async fn refresh_tree(state: &AppState) -> anyhow::Result<Hash> {
    let repository = state.repository.clone();
    let versioned = state.versioned;
    let revision =
        with_lore_ctx(async move { served_revision(&repository, versioned).await }).await?;

    if revision == state.revision().await {
        return Ok(revision); // nothing changed; keep the current handle
    }

    // Load a fresh tree handle for the new revision.
    let tree_sink: Arc<Mutex<Option<LoreRevisionTree>>> = Arc::new(Mutex::new(None));
    let tree_cb = tree_sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeLoaded(data) = event {
            *tree_cb.lock().unwrap() = Some(LoreRevisionTree { handle_id: data.handle_id });
        }
    }));

    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: state.store,
            repository: state.repository_id,
            revision_hash: revision,
        },
        callback,
    )
    .await;

    let new_tree = tree_sink.lock().unwrap().take();
    let new_tree = match new_tree {
        Some(t) if status == 0 => t,
        _ => anyhow::bail!("revision tree reload failed (status {status})"),
    };

    // Swap and close the previous handle (best effort).
    let old = {
        let mut guard = state.tree_state.write().await;
        let old = guard.tree;
        *guard = TreeState { tree: new_tree, revision };
        old
    };
    // Node ids (and therefore materialized addresses) belong to the old handle.
    state.addr_cache.write().await.clear();
    let close_status = tree_close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs { id: 0, handle: old },
        None,
    )
    .await;
    if close_status != 0 {
        warn!("closing stale revision tree handle failed (status {close_status})");
    }

    info!("Refreshed tree to revision {revision}");
    Ok(revision)
}

/// Build the full node record served by GET/PATCH node endpoints.
async fn node_record(
    state: &AppState,
    tree: LoreRevisionTree,
    node_id: u32,
) -> Result<NodeResponse, ApiError> {
    let info = fetch_node_info(tree, node_id).await?;
    let path = fetch_node_path(tree, node_id).await?;
    let address =
        effective_address_str(state, tree, node_id, info.kind, info.size, info.address).await;
    Ok(NodeResponse {
        node_id: info.node_id as u64,
        parent_id: info.parent_id as u64,
        name: info.name.as_str().to_owned(),
        kind: kind_str(info.kind).to_owned(),
        mode: info.mode,
        size: info.size,
        address,
        path,
    })
}

// ─── Read handlers ───────────────────────────────────────────────────────────

/// `GET /api/v1/info`
async fn handle_info(Extension(state): Extension<Arc<AppState>>) -> Json<InfoResponse> {
    Json(InfoResponse {
        repository_id: state.repository_id.to_string(),
        branch_id: state.branch_id.to_string(),
        branch_name: state.branch_name.clone(),
        revision: state.revision().await.to_string(),
        workdir: state.workdir.display().to_string(),
        mode: state.mode_str().to_owned(),
    })
}

/// Query parameters for `GET /api/v1/tree`
#[derive(Deserialize)]
struct TreeQuery {
    node_id: Option<u64>,
}

/// `GET /api/v1/tree?node_id=<u64>`
async fn handle_tree(
    Extension(state): Extension<Arc<AppState>>,
    Query(params): Query<TreeQuery>,
) -> Result<Json<TreeResponse>, ApiError> {
    let parent_node_id = parse_node_id(params.node_id.unwrap_or(ROOT_NODE as u64))?;
    let tree = state.tree().await;
    let (repository_id, revision, children) = fetch_children(tree, parent_node_id).await?;

    let mut entries: Vec<ChildEntry> = Vec::with_capacity(children.len());
    for c in &children {
        // Note: children listed *through a link* belong to another repository;
        // address materialization only applies to this workspace's own files
        // (the workdir path is only meaningful there), so pass raw addresses
        // through when the listing crossed into a linked repo.
        let address = if repository_id == state.repository_id {
            effective_address_str(&state, tree, c.node_id, c.kind, c.size, c.address).await
        } else {
            address_opt(c.kind, c.address)
        };
        entries.push(ChildEntry {
            node_id: c.node_id as u64,
            name: c.name.as_str().to_owned(),
            kind: kind_str(c.kind).to_owned(),
            mode: c.mode,
            size: c.size,
            address,
        });
    }

    Ok(Json(TreeResponse {
        repository_id: repository_id.to_string(),
        revision: revision.to_string(),
        node_id: parent_node_id as u64,
        children: entries,
    }))
}

/// `GET /api/v1/node/{node_id}`
async fn handle_node(
    Extension(state): Extension<Arc<AppState>>,
    Path(node_id_str): Path<String>,
) -> Result<Json<NodeResponse>, ApiError> {
    let node_id = parse_node_id(
        node_id_str
            .parse()
            .map_err(|_| err_resp(StatusCode::BAD_REQUEST, "node_id must be a u64"))?,
    )?;
    let tree = state.tree().await;
    Ok(Json(node_record(&state, tree, node_id).await?))
}

/// Fetch a file's bytes: CAS first (exact displayed address), falling back to
/// the working directory (covers staged-but-not-yet-put content, e.g. files
/// staged by an external `lore` CLI while lore-drive runs in drive mode).
///
/// A zero content hash is **not** sent to the CAS: `get_file`'s documented
/// contract for `hash == default` is *truncate to zero bytes and succeed*,
/// which would silently serve empty content for staged-not-yet-hashed files.
/// Non-empty files with a zero-hash record read straight from the workdir.
async fn read_file_bytes(
    state: &AppState,
    partition: RepositoryId,
    address: Address,
    virtual_path: &str,
    scratch: &FsPath,
) -> anyhow::Result<Vec<u8>> {
    if address.hash == Hash::default() {
        let fs_path = state.workdir.join(rel_of(virtual_path));
        return match tokio::fs::read(&fs_path).await {
            Ok(bytes) => Ok(bytes),
            // Missing workdir file + zero hash: honor lore's empty-content convention.
            Err(_) => Ok(Vec::new()),
        };
    }
    match cas_fetch_to_file(state.store, partition, address, scratch).await {
        Ok(()) => Ok(tokio::fs::read(scratch).await?),
        Err(cas_err) => {
            let fs_path = state.workdir.join(rel_of(virtual_path));
            match tokio::fs::read(&fs_path).await {
                Ok(bytes) => {
                    warn!("CAS miss for {virtual_path} ({cas_err}); served from workdir");
                    Ok(bytes)
                }
                Err(fs_err) => anyhow::bail!("CAS: {cas_err}; workdir: {fs_err}"),
            }
        }
    }
}

/// `GET /api/v1/download/{node_id}` — file bytes or folder ZIP.
async fn handle_download(
    Extension(state): Extension<Arc<AppState>>,
    Path(node_id_str): Path<String>,
) -> Result<Response, ApiError> {
    let node_id = parse_node_id(
        node_id_str
            .parse()
            .map_err(|_| err_resp(StatusCode::BAD_REQUEST, "node_id must be a u64"))?,
    )?;
    let tree = state.tree().await;

    // Root has no node_info record of its own; treat it as a directory named "root".
    let (kind, name, address, size) = if node_id == ROOT_NODE {
        (LoreNodeType::Directory as u32, "root".to_owned(), Address::default(), 0u64)
    } else {
        let info = fetch_node_info(tree, node_id).await?;
        (info.kind, info.name.as_str().to_owned(), info.address, info.size)
    };

    if is_link(kind) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "link download is not supported"));
    }

    let tmp = tempfile::tempdir()
        .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, format!("tempdir: {e}")))?;
    let virtual_root = fetch_node_path(tree, node_id).await?;

    if is_file(kind) {
        // Materialize the authoritative address first (drive mode: staged
        // records carry a zero hash until content is pushed into the CAS).
        let address = effective_address(&state, tree, node_id, kind, size, address)
            .await
            .unwrap_or(address);
        let scratch = tmp.path().join("payload");
        let bytes = read_file_bytes(&state, state.repository_id, address, &virtual_root, &scratch)
            .await
            .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok(binary_response(bytes, "application/octet-stream", &name));
    }

    // Directory → walk the served subtree, fetch files, zip.
    let mut files: Vec<(String, PathBuf)> = Vec::new(); // (zip rel path, temp path)
    let mut dirs: Vec<String> = Vec::new();
    let mut stack: Vec<(u32, String)> = vec![(node_id, String::new())];
    let mut counter: u64 = 0;

    while let Some((dir_id, prefix)) = stack.pop() {
        let (repo, _rev, children) = fetch_children(tree, dir_id).await?;
        for child in children {
            let child_name = child.name.as_str().to_owned();
            let rel = if prefix.is_empty() {
                child_name.clone()
            } else {
                format!("{prefix}/{child_name}")
            };
            if is_dir(child.kind) {
                dirs.push(rel.clone());
                stack.push((child.node_id, rel));
            } else if is_file(child.kind) {
                counter += 1;
                let scratch = tmp.path().join(format!("f{counter}"));
                let child_virtual = join_virtual(&virtual_root, &rel);
                let bytes =
                    read_file_bytes(&state, repo, child.address, &child_virtual, &scratch)
                        .await
                        .map_err(|e| {
                            err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                        })?;
                let staged = tmp.path().join(format!("z{counter}"));
                tokio::fs::write(&staged, &bytes)
                    .await
                    .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                files.push((rel, staged));
            } else {
                // Links are skipped in ZIP archives (v1).
            }
        }
    }

    let zip_bytes = tokio::task::spawn_blocking(move || build_zip(&dirs, &files))
        .await
        .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(binary_response(zip_bytes, "application/zip", &format!("{name}.zip")))
}

/// Assemble a ZIP archive from directory entries and (rel path, file) pairs.
fn build_zip(dirs: &[String], files: &[(String, PathBuf)]) -> anyhow::Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for dir in dirs {
            zip.add_directory(dir.as_str(), options)?;
        }
        for (rel, src) in files {
            zip.start_file(rel.as_str(), options)?;
            let bytes = std::fs::read(src)?;
            zip.write_all(&bytes)?;
        }
        zip.finish()?;
    }
    Ok(cursor.into_inner())
}

/// Build an attachment response with content-type / disposition headers.
fn binary_response(bytes: Vec<u8>, content_type: &str, filename: &str) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    // Quote-escape the filename; fall back to a generic name on weird input.
    let disposition = format!("attachment; filename=\"{}\"", filename.replace('"', "_"));
    let disposition = HeaderValue::from_str(&disposition)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""));
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    (StatusCode::OK, headers, bytes).into_response()
}

// ─── Write handlers ──────────────────────────────────────────────────────────

/// Resolve a directory node to its "/"-prefixed virtual path, validating kind.
async fn dir_virtual_path(tree: LoreRevisionTree, node_id: u32) -> Result<String, ApiError> {
    if node_id == ROOT_NODE {
        return Ok("/".to_owned());
    }
    let info = fetch_node_info(tree, node_id).await?;
    if !is_dir(info.kind) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "node is not a directory"));
    }
    fetch_node_path(tree, node_id).await
}

#[derive(Deserialize)]
struct MkdirRequest {
    #[serde(default)]
    parent_id: u64,
    name: String,
}

/// `POST /api/v1/mkdir`
async fn handle_mkdir(
    Extension(state): Extension<Arc<AppState>>,
    Json(req): Json<MkdirRequest>,
) -> Result<(StatusCode, Json<MkdirResponse>), ApiError> {
    if !valid_name(&req.name) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid directory name"));
    }
    let parent_id = parse_node_id(req.parent_id)?;

    let _write = state.write_gate.lock().await;
    let tree = state.tree().await;
    let prev_revision = state.revision().await;
    let parent_path = dir_virtual_path(tree, parent_id).await?;
    let virtual_path = join_virtual(&parent_path, &req.name);
    let rel = rel_of(&virtual_path).to_owned();
    let fs_path = state.workdir.join(&rel);

    if try_resolve_path(tree, &rel).await.is_some() || fs_path.exists() {
        return Err(conflict_resp("an entry with that name already exists", vec![virtual_path]));
    }

    tokio::fs::create_dir(&fs_path)
        .await
        .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, format!("create_dir: {e}")))?;

    // Try to stage the bare directory; fall back to a `.lorekeep` placeholder
    // when staging records nothing for an empty directory (unchanged staged
    // revision in drive mode / NothingStaged commit error in versioned mode).
    let commit_msg = format!("lore-drive: mkdir {virtual_path}");
    let result: anyhow::Result<Hash> = async {
        let staged = stage_paths(vec![rel.clone()]).await?;
        if !state.versioned && staged == prev_revision {
            anyhow::bail!("empty directory staged nothing");
        }
        finalize_mutation(&state, &commit_msg).await
    }
    .await;

    let result = match result {
        Ok(rev) => Ok(rev),
        Err(first_err) => {
            info!("empty-dir stage fell back to .lorekeep ({first_err})");
            let keep_rel = format!("{rel}/.lorekeep");
            let keep_fs = state.workdir.join(&keep_rel);
            match tokio::fs::write(&keep_fs, b"").await {
                Ok(()) => async {
                    stage_paths(vec![keep_rel.clone()]).await?;
                    finalize_mutation(&state, &commit_msg).await
                }
                .await,
                Err(e) => Err(anyhow::anyhow!("write .lorekeep: {e}")),
            }
        }
    };

    let revision = match result {
        Ok(rev) => rev,
        Err(e) => {
            // Roll back the filesystem change (best effort).
            let _ = tokio::fs::remove_dir_all(&fs_path).await;
            return Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    let new_tree = state.tree().await;
    let node_id = try_resolve_path(new_tree, &rel).await.map(|n| n as u64);

    Ok((
        StatusCode::CREATED,
        Json(MkdirResponse {
            node_id,
            path: virtual_path,
            revision: revision.to_string(),
        }),
    ))
}

#[derive(Deserialize)]
struct UploadQuery {
    #[serde(default)]
    parent_id: u64,
    #[serde(default)]
    overwrite: bool,
}

/// `POST /api/v1/upload?parent_id=<u64>&overwrite=<bool>`
async fn handle_upload(
    Extension(state): Extension<Arc<AppState>>,
    Query(params): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), ApiError> {
    let parent_id = parse_node_id(params.parent_id)?;

    let _write = state.write_gate.lock().await;
    let tree = state.tree().await;
    let parent_path = dir_virtual_path(tree, parent_id).await?;

    // 1. Buffer every part into a temp dir before touching the workspace.
    let tmp = tempfile::tempdir()
        .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, format!("tempdir: {e}")))?;
    let mut incoming: Vec<(Vec<String>, PathBuf, u64)> = Vec::new(); // (segments, temp path, size)
    let mut counter: u64 = 0;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err_resp(StatusCode::BAD_REQUEST, format!("malformed multipart: {e}")))?
    {
        let Some(filename) = field.file_name().map(str::to_owned) else {
            continue; // ignore non-file fields
        };
        let segments = sanitize_rel_path(&filename)
            .ok_or_else(|| err_resp(StatusCode::BAD_REQUEST, format!("illegal path: {filename}")))?;
        let data = field
            .bytes()
            .await
            .map_err(|e| err_resp(StatusCode::BAD_REQUEST, format!("read part: {e}")))?;
        counter += 1;
        let temp_path = tmp.path().join(format!("u{counter}"));
        let size = data.len() as u64;
        tokio::fs::write(&temp_path, &data)
            .await
            .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        incoming.push((segments, temp_path, size));
    }

    if incoming.is_empty() {
        return Err(err_resp(StatusCode::BAD_REQUEST, "no file parts in request"));
    }

    // 2. Conflict detection against the served tree, before any change.
    //    Re-uploading the same path is an *update in place* when
    //    `overwrite=true` (drive semantics: one copy per path, ever).
    let mut conflicts: Vec<String> = Vec::new();
    for (segments, _, _) in &incoming {
        let virtual_path = segments
            .iter()
            .fold(parent_path.clone(), |acc, seg| join_virtual(&acc, seg));
        let rel = rel_of(&virtual_path).to_owned();
        if let Some(existing) = try_resolve_path(tree, &rel).await {
            match fetch_node_info(tree, existing).await {
                Ok(info) if is_file(info.kind) && params.overwrite => {} // update in place
                Ok(_) if !params.overwrite => conflicts.push(virtual_path),
                Ok(info) if !is_file(info.kind) => {
                    return Err(err_resp(
                        StatusCode::BAD_REQUEST,
                        format!("{virtual_path} exists and is not a file"),
                    ));
                }
                _ => {}
            }
        }
    }
    if !conflicts.is_empty() {
        return Err(conflict_resp(
            format!("{} path(s) already exist", conflicts.len()),
            conflicts,
        ));
    }

    // 3. Materialize into the workspace, then stage (+ commit when versioned).
    let mut rel_paths: Vec<String> = Vec::new();
    let mut placed: Vec<PathBuf> = Vec::new();
    for (segments, temp_path, _) in &incoming {
        let virtual_path = segments
            .iter()
            .fold(parent_path.clone(), |acc, seg| join_virtual(&acc, seg));
        let rel = rel_of(&virtual_path).to_owned();
        let fs_path = state.workdir.join(&rel);
        if let Some(dir) = fs_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(dir).await {
                rollback_files(&placed).await;
                return Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        }
        if let Err(e) = tokio::fs::copy(temp_path, &fs_path).await {
            rollback_files(&placed).await;
            return Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
        placed.push(fs_path);
        rel_paths.push(rel);
    }

    let commit_msg = format!(
        "lore-drive: upload {} file(s) to {parent_path}",
        rel_paths.len()
    );
    let finalized = async {
        stage_paths(rel_paths.clone()).await?;
        finalize_mutation(&state, &commit_msg).await
    }
    .await;
    let revision = match finalized {
        Ok(rev) => rev,
        Err(e) => {
            rollback_files(&placed).await;
            return Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    // 4. Resolve the created nodes; in drive mode also push each file's
    //    content into the CAS at the staged node's `file_id` context —
    //    commit is what would normally hash + store content, and we never
    //    commit.  The staged node record keeps a **zero** content hash (see
    //    `effective_address`), so the put's returned address is the
    //    authoritative one: it goes into the response and the address cache.
    //    put_file dedups by content hash, so re-uploading identical bytes
    //    stores nothing new.
    let new_tree = state.tree().await;
    let mut files = Vec::new();
    for ((segments, _, size), rel) in incoming.iter().zip(rel_paths.iter()) {
        let node_id = try_resolve_path(new_tree, rel).await;
        let mut address_str = None;
        if let Some(id) = node_id {
            if let Ok(node) = fetch_node_info(new_tree, id).await {
                address_str = address_opt(node.kind, node.address);
                if !state.versioned && is_file(node.kind) {
                    let src = state.workdir.join(rel);
                    match cas_put_file(state.store, state.repository_id, node.address.context, &src)
                        .await
                    {
                        Ok(stored) => {
                            if stored.context != node.address.context {
                                warn!(
                                    "CAS file_id mismatch for /{rel}: staged {} vs stored {}",
                                    node.address.context, stored.context
                                );
                            }
                            state.addr_cache.write().await.insert(id, stored);
                            address_str = Some(stored.to_string());
                        }
                        Err(e) => warn!("CAS put_file failed for /{rel}: {e}"),
                    }
                }
            }
        }
        files.push(UploadedFile {
            name: segments.last().cloned().unwrap_or_default(),
            path: format!("/{rel}"),
            node_id: node_id.map(|n| n as u64),
            size: *size,
            address: address_str,
        });
    }

    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            revision: revision.to_string(),
            files,
        }),
    ))
}

/// Best-effort removal of files placed in the workspace before a failure.
async fn rollback_files(placed: &[PathBuf]) {
    for path in placed {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[derive(Deserialize)]
struct PatchRequest {
    name: Option<String>,
    parent_id: Option<u64>,
}

/// `PATCH /api/v1/node/{node_id}` — rename and/or move.
async fn handle_node_patch(
    Extension(state): Extension<Arc<AppState>>,
    Path(node_id_str): Path<String>,
    Json(req): Json<PatchRequest>,
) -> Result<Json<PatchResponse>, ApiError> {
    let node_id = parse_node_id(
        node_id_str
            .parse()
            .map_err(|_| err_resp(StatusCode::BAD_REQUEST, "node_id must be a u64"))?,
    )?;
    if node_id == ROOT_NODE {
        return Err(err_resp(StatusCode::BAD_REQUEST, "cannot rename or move the root"));
    }
    if req.name.is_none() && req.parent_id.is_none() {
        return Err(err_resp(StatusCode::BAD_REQUEST, "provide name and/or parent_id"));
    }
    if let Some(name) = &req.name {
        if !valid_name(name) {
            return Err(err_resp(StatusCode::BAD_REQUEST, "invalid name"));
        }
    }

    let _write = state.write_gate.lock().await;
    let tree = state.tree().await;
    let info = fetch_node_info(tree, node_id).await?;
    let src_virtual = fetch_node_path(tree, node_id).await?;

    let dst_parent_id = match req.parent_id {
        Some(p) => parse_node_id(p)?,
        None => info.parent_id,
    };
    let dst_parent_path = dir_virtual_path(tree, dst_parent_id).await?;
    let dst_name = req.name.clone().unwrap_or_else(|| info.name.as_str().to_owned());
    let dst_virtual = join_virtual(&dst_parent_path, &dst_name);

    if dst_virtual == src_virtual {
        // No-op rename: return the current record without staging anything.
        let node = node_record(&state, tree, node_id).await?;
        return Ok(Json(PatchResponse {
            node,
            revision: state.revision().await.to_string(),
        }));
    }
    if is_dir(info.kind) && dst_virtual.starts_with(&format!("{src_virtual}/")) {
        return Err(err_resp(
            StatusCode::BAD_REQUEST,
            "cannot move a directory inside itself",
        ));
    }

    let src_rel = rel_of(&src_virtual).to_owned();
    let dst_rel = rel_of(&dst_virtual).to_owned();
    let src_fs = state.workdir.join(&src_rel);
    let dst_fs = state.workdir.join(&dst_rel);

    if try_resolve_path(tree, &dst_rel).await.is_some() || dst_fs.exists() {
        return Err(conflict_resp(
            "destination already exists",
            vec![dst_virtual],
        ));
    }

    tokio::fs::rename(&src_fs, &dst_fs)
        .await
        .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")))?;

    let commit_msg = format!("lore-drive: move {src_virtual} -> {dst_virtual}");
    let finalized = async {
        stage_move_path(&src_rel, &dst_rel).await?;
        finalize_mutation(&state, &commit_msg).await
    }
    .await;
    let revision = match finalized {
        Ok(rev) => rev,
        Err(e) => {
            let _ = tokio::fs::rename(&dst_fs, &src_fs).await; // roll back
            return Err(err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    let new_tree = state.tree().await;
    let new_id = try_resolve_path(new_tree, &dst_rel)
        .await
        .ok_or_else(|| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "moved node not found"))?;
    let node = node_record(&state, new_tree, new_id).await?;

    Ok(Json(PatchResponse {
        node,
        revision: revision.to_string(),
    }))
}

/// `DELETE /api/v1/node/{node_id}`
async fn handle_node_delete(
    Extension(state): Extension<Arc<AppState>>,
    Path(node_id_str): Path<String>,
) -> Result<Json<DeleteResponse>, ApiError> {
    let node_id = parse_node_id(
        node_id_str
            .parse()
            .map_err(|_| err_resp(StatusCode::BAD_REQUEST, "node_id must be a u64"))?,
    )?;
    if node_id == ROOT_NODE {
        return Err(err_resp(StatusCode::BAD_REQUEST, "cannot delete the root"));
    }

    let _write = state.write_gate.lock().await;
    let tree = state.tree().await;
    let info = fetch_node_info(tree, node_id).await?;
    let virtual_path = fetch_node_path(tree, node_id).await?;
    let rel = rel_of(&virtual_path).to_owned();
    let fs_path = state.workdir.join(&rel);

    if is_dir(info.kind) {
        tokio::fs::remove_dir_all(&fs_path).await
    } else {
        tokio::fs::remove_file(&fs_path).await
    }
    .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, format!("remove: {e}")))?;

    let commit_msg = format!("lore-drive: delete {virtual_path}");
    let revision = async {
        stage_paths(vec![rel.clone()]).await?;
        finalize_mutation(&state, &commit_msg).await
    }
    .await
    .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(DeleteResponse {
        revision: revision.to_string(),
    }))
}

// ─── Startup ─────────────────────────────────────────────────────────────────

/// Open the lore workspace and build the shared [`AppState`].
///
/// **Must be called inside a `LORE_CONTEXT.scope`** because
/// `load_and_connect` (and other lore verbs) call `execution_context()`
/// internally, which panics if the task-local is absent.
async fn open_workspace(workdir: &FsPath, versioned: bool) -> anyhow::Result<AppState> {
    // Open a read-only repository context. Write verbs (`stage`, `commit`)
    // acquire their own per-call write token; keeping the long-lived context
    // read-only avoids holding the workspace write mutex between requests.
    let repository = load_and_connect(workdir, RepositoryAccess::ReadOnly).await?;

    // Anchor identities (branch) + the revision this instance serves:
    // drive mode prefers an existing staged revision over the committed tip.
    let (_current, branch_id) = lore_revision::instance::load_current_anchor(&repository).await?;
    let revision = served_revision(&repository, versioned).await?;
    info!(
        "Serving revision: {revision}  branch: {branch_id}  mode: {}",
        if versioned { "versioned" } else { "drive" }
    );

    // Resolve the human-readable branch name (best-effort; empty on failure).
    let branch_name = lore_revision::branch::metadata_local(repository.clone(), branch_id)
        .await
        .ok()
        .and_then(|meta| lore_revision::branch::name(&meta).ok().map(str::to_owned))
        .unwrap_or_default();
    info!("Branch name: {branch_name:?}");

    // Open the content-addressed storage handle.
    let store_handle: Arc<Mutex<LoreStore>> = Arc::new(Mutex::new(LoreStore::INVALID));
    let store_handle_cb = store_handle.clone();
    let store_callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::StorageOpened(data) = event {
            *store_handle_cb.lock().unwrap() = LoreStore { handle_id: data.handle_id };
        }
    }));

    let open_status = storage_open(
        LoreGlobalArgs::default(),
        LoreStorageOpenArgs {
            repository_path: LoreString::from(workdir.to_string_lossy().as_ref()),
            ..Default::default()
        },
        store_callback,
    )
    .await;

    if open_status != 0 {
        anyhow::bail!("lore_storage_open failed (status {open_status})");
    }

    let store = *store_handle.lock().unwrap();
    if store == LoreStore::INVALID {
        anyhow::bail!("lore_storage_open succeeded but emitted no handle");
    }
    info!("Storage handle opened (id={})", store.handle_id);

    // Load the revision tree for the served revision.
    let tree_handle: Arc<Mutex<Option<LoreRevisionTree>>> = Arc::new(Mutex::new(None));
    let tree_handle_cb = tree_handle.clone();
    let load_callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeLoaded(data) = event {
            *tree_handle_cb.lock().unwrap() =
                Some(LoreRevisionTree { handle_id: data.handle_id });
        }
    }));

    let load_status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store,
            repository: repository.id,
            revision_hash: revision,
        },
        load_callback,
    )
    .await;

    if load_status != 0 {
        anyhow::bail!("lore_revision_tree_load failed (status {load_status})");
    }

    let tree = tree_handle
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("load succeeded but emitted no tree handle"))?;
    info!("Revision tree loaded (handle_id={})", tree.handle_id);

    Ok(AppState {
        repository_id: repository.id,
        repository,
        store,
        tree_state: tokio::sync::RwLock::new(TreeState { tree, revision }),
        write_gate: tokio::sync::Mutex::new(()),
        versioned,
        addr_cache: tokio::sync::RwLock::new(HashMap::new()),
        branch_id,
        branch_name,
        workdir: workdir.to_path_buf(),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured logging — controlled by RUST_LOG env var.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lore_drive=info,tower_http=debug".into()),
        )
        .init();

    let cli = Cli::parse();

    // ── Locate the workspace ────────────────────────────────────────────────
    let workdir = env::current_dir()?;
    info!("Opening lore workspace at {}", workdir.display());

    // ── Build an ExecutionContext and enter its LORE_CONTEXT scope ──────────
    //
    // lore verbs (including `load_and_connect`) call `execution_context()`
    // internally, which reads from a tokio task-local.  We must establish
    // that scope before calling *any* lore API.
    //
    // For startup we don't need a real event callback — `no_dispatch()` is
    // fine; each individual request handler creates its own scoped context
    // through the internal `storage_call` / `revision_tree_call` dispatch
    // helpers (and `refresh_tree` wraps raw calls in `with_lore_ctx`).
    let startup_ctx: Arc<dyn std::any::Any + Send + Sync> = Arc::new(
        ExecutionContext::new_client(LoreGlobalArgs::default(), EventDispatcher::no_dispatch()),
    );

    let state = LORE_CONTEXT
        .scope(startup_ctx, open_workspace(&workdir, cli.versioned))
        .await?;

    let state = Arc::new(state);

    // ── Build Axum router ────────────────────────────────────────────────────
    let app = Router::new()
        .route("/api/v1/info", get(handle_info))
        .route("/api/v1/tree", get(handle_tree))
        .route(
            "/api/v1/node/{node_id}",
            get(handle_node)
                .patch(handle_node_patch)
                .delete(handle_node_delete),
        )
        .route("/api/v1/download/{node_id}", get(handle_download))
        .route("/api/v1/mkdir", post(handle_mkdir))
        .route("/api/v1/upload", post(handle_upload))
        // Uploads can be large — allow up to 1 GiB bodies.
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        // Allow the future SvelteKit frontend (different port) to call us in dev.
        .layer(CorsLayer::permissive())
        .layer(Extension(state));

    // ── Listen ───────────────────────────────────────────────────────────────
    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    info!("lore-drive listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
