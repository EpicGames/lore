// SPDX-FileCopyrightText: Nicolas Sauzede <nicolas.sauzede@gmail.com>
// SPDX-License-Identifier: MIT
//! `lore-drive` — Axum/Tokio REST backend that exposes a lore workspace as a
//! browsable file/folder tree over HTTP.
//!
//! # Endpoints
//!
//! - `GET /api/v1/info`               — workspace metadata
//! - `GET /api/v1/tree?node_id=<u64>` — directory listing
//! - `GET /api/v1/node/{node_id}`      — single-node record + full path
//!
//! See `REST_API.md` for the authoritative JSON shapes.
//!
//! # Usage
//!
//! Run inside a lore workspace (same working directory where you would invoke
//! the `lore` CLI):
//!
//! ```sh
//! lore-drive               # listen on 0.0.0.0:8080
//! lore-drive --port 9090   # custom port
//! ```

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Extension;
use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::get;
use clap::Parser;
use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
use lore::revision_tree::list_children::list_children;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
use lore::revision_tree::node_info::node_info;
use lore::revision_tree::node_path::LoreRevisionTreeNodePathArgs;
use lore::revision_tree::node_path::node_path;
use lore::storage::handle::LoreStore;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::open as storage_open;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeChildEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeNodePathEventData;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::load_and_connect;
use lore::revision_tree::handle::LoreRevisionTree;
use serde::Deserialize;
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tracing::info;

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "lore-drive", about = "Lore workspace REST backend")]
struct Cli {
    /// TCP port to listen on
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

// ─── App state ───────────────────────────────────────────────────────────────

/// Shared state injected into every handler via `axum::Extension`.
struct AppState {
    /// Open repository context (kept alive so the underlying stores remain open).
    #[allow(dead_code)]
    repository: Arc<RepositoryContext>,
    /// Loaded revision-tree handle (the current committed revision).
    tree: LoreRevisionTree,
    /// Repository identity.
    repository_id: RepositoryId,
    /// Identity of the active branch.
    branch_id: BranchId,
    /// Human-readable name of the active branch (may be empty for detached).
    branch_name: String,
    /// Latest committed revision hash.
    revision: Hash,
    /// Absolute path of the workspace root.
    workdir: String,
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

/// `GET /api/v1/node/{node_id}` response.
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

/// Uniform error body for 4xx / 5xx responses.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn err_resp(
    status: StatusCode,
    msg: impl Into<String>,
) -> (StatusCode, Json<ErrorBody>) {
    (status, Json(ErrorBody { error: msg.into() }))
}

// ─── Helper: node kind ────────────────────────────────────────────────────────

fn kind_str(kind: u32) -> &'static str {
    if kind == LoreNodeType::File as u32 {
        "file"
    } else if kind == LoreNodeType::Link as u32 {
        "link"
    } else {
        "directory"
    }
}

/// Returns `None` for directories, `Some(address.to_string())` for files/links.
fn address_opt(kind: u32, address: lore_base::types::Address) -> Option<String> {
    if kind == LoreNodeType::Directory as u32 {
        None
    } else {
        Some(address.to_string())
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// `GET /api/v1/info`
async fn handle_info(
    Extension(state): Extension<Arc<AppState>>,
) -> Json<InfoResponse> {
    Json(InfoResponse {
        repository_id: state.repository_id.to_string(),
        branch_id: state.branch_id.to_string(),
        branch_name: state.branch_name.clone(),
        revision: state.revision.to_string(),
        workdir: state.workdir.clone(),
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
) -> Result<Json<TreeResponse>, (StatusCode, Json<ErrorBody>)> {
    // node_id == 0 or absent → ROOT_NODE (ROOT_NODE == 0 in lore)
    let raw = params.node_id.unwrap_or(0);
    if raw > u32::MAX as u64 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "node_id out of u32 range"));
    }
    let parent_node_id: u32 = raw as u32;

    // Collect events emitted by the callback-based `list_children` API.
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

    let sink_cb = sink.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
        LoreEvent::RevisionTreeListChildrenBegin(data) => {
            let mut s = sink_cb.lock().unwrap();
            s.repository_id = data.repository;
            s.revision = data.revision;
            s.begin_error = data.error_code;
        }
        LoreEvent::RevisionTreeChild(data) => {
            sink_cb.lock().unwrap().children.push(data.clone());
        }
        _ => {}
    }));

    let status = list_children(
        LoreGlobalArgs::default(),
        LoreRevisionTreeListChildrenArgs {
            id: 1,
            handle: state.tree,
            parent_node_id,
        },
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

    let children: Vec<ChildEntry> = s
        .children
        .iter()
        .map(|c| ChildEntry {
            node_id: c.node_id as u64,
            name: c.name.as_str().to_owned(),
            kind: kind_str(c.kind).to_owned(),
            mode: c.mode,
            size: c.size,
            address: address_opt(c.kind, c.address),
        })
        .collect();

    Ok(Json(TreeResponse {
        repository_id: s.repository_id.to_string(),
        revision: s.revision.to_string(),
        node_id: parent_node_id as u64,
        children,
    }))
}

/// `GET /api/v1/node/{node_id}`
async fn handle_node(
    Extension(state): Extension<Arc<AppState>>,
    Path(node_id_str): Path<String>,
) -> Result<Json<NodeResponse>, (StatusCode, Json<ErrorBody>)> {
    let node_id_u64: u64 = node_id_str
        .parse()
        .map_err(|_| err_resp(StatusCode::BAD_REQUEST, "node_id must be a u64"))?;

    if node_id_u64 > u32::MAX as u64 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "node_id out of u32 range"));
    }
    let node_id = node_id_u64 as u32;

    if node_id == INVALID_NODE {
        return Err(err_resp(
            StatusCode::NOT_FOUND,
            "node_id is the invalid sentinel",
        ));
    }

    // ── node_info ────────────────────────────────────────────────────────────
    let info_sink: Arc<Mutex<Option<LoreRevisionTreeNodeInfoEventData>>> =
        Arc::new(Mutex::new(None));
    let info_cb = info_sink.clone();
    let info_callback: LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RevisionTreeNodeInfo(data) = event {
                *info_cb.lock().unwrap() = Some(data.clone());
            }
        }));

    let info_status = node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs {
            id: 2,
            handle: state.tree,
            node_id,
        },
        info_callback,
    )
    .await;

    let info_data = info_sink.lock().unwrap().clone();
    let info_data = match info_data {
        Some(d) if info_status == 0 && d.error_code == LoreErrorCode::None => d,
        Some(d) if d.error_code == LoreErrorCode::InvalidArguments => {
            return Err(err_resp(StatusCode::NOT_FOUND, "node not found"));
        }
        _ => {
            return Err(err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "node_info failed",
            ));
        }
    };

    // ── node_path ────────────────────────────────────────────────────────────
    let path_sink: Arc<Mutex<Option<LoreRevisionTreeNodePathEventData>>> =
        Arc::new(Mutex::new(None));
    let path_cb = path_sink.clone();
    let path_callback: LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RevisionTreeNodePath(data) = event {
                *path_cb.lock().unwrap() = Some(data.clone());
            }
        }));

    let path_status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 3,
            handle: state.tree,
            node_id,
        },
        path_callback,
    )
    .await;

    let path_data = path_sink.lock().unwrap().clone();
    let full_path = match path_data {
        Some(d) if path_status == 0 && d.error_code == LoreErrorCode::None => {
            // Prepend "/" so every path starts at root; the root itself becomes "/".
            let raw = d.path.as_str();
            if raw.is_empty() { "/".to_owned() } else { format!("/{raw}") }
        }
        _ => {
            return Err(err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "node_path failed",
            ));
        }
    };

    Ok(Json(NodeResponse {
        node_id: info_data.node_id as u64,
        parent_id: info_data.parent_id as u64,
        name: info_data.name.as_str().to_owned(),
        kind: kind_str(info_data.kind).to_owned(),
        mode: info_data.mode,
        size: info_data.size,
        address: address_opt(info_data.kind, info_data.address),
        path: full_path,
    }))
}

// ─── Startup ─────────────────────────────────────────────────────────────────

/// Open the lore workspace and build the shared [`AppState`].
///
/// **Must be called inside a `LORE_CONTEXT.scope`** because
/// `load_and_connect` (and other lore verbs) call `execution_context()`
/// internally, which panics if the task-local is absent.
async fn open_workspace(workdir: &std::path::Path) -> anyhow::Result<AppState> {
    // Open a read-only repository context.
    let repository = load_and_connect(workdir, RepositoryAccess::ReadOnly).await?;

    // Read the current anchor (revision hash + branch id).
    let (revision, branch_id) =
        lore_revision::instance::load_current_anchor(&repository).await?;
    info!("Active revision: {revision}  branch: {branch_id}");

    // Resolve the human-readable branch name (best-effort; empty on failure).
    let branch_name =
        lore_revision::branch::metadata_local(repository.clone(), branch_id)
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

    // Load the revision tree for the current committed revision.
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
        tree,
        branch_id,
        branch_name,
        revision,
        workdir: workdir.display().to_string(),
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
    // through the internal `storage_call` / `revision_tree_call` helpers.
    let startup_ctx: Arc<dyn std::any::Any + Send + Sync> = Arc::new(
        ExecutionContext::new_client(LoreGlobalArgs::default(), EventDispatcher::no_dispatch()),
    );

    let state = LORE_CONTEXT
        .scope(startup_ctx, open_workspace(&workdir))
        .await?;

    let state = Arc::new(state);

    // ── Build Axum router ────────────────────────────────────────────────────
    let app = Router::new()
        .route("/api/v1/info", get(handle_info))
        .route("/api/v1/tree", get(handle_tree))
        .route("/api/v1/node/{node_id}", get(handle_node))
        // Allow the future SvelteKit frontend (different port) to call us in dev.
        .layer(CorsLayer::permissive())
        .layer(Extension(state));

    // ── Listen ───────────────────────────────────────────────────────────────
    //let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    info!("lore-drive listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
