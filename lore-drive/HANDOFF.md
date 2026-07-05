# HANDOFF

## 1. Setup

Firstly, apply RUST.md to correctly setup the required Rust toolchain.
(the workspace uses edition 2024 which requires Rust ≥ 1.85):

Verify the workspace root `Cargo.toml` references `lore-drive` in `members` —
it already does as of the commit that accompanies this HANDOFF.

## 2. Tasks (see below)

Then I want you to perform the tasks not yet done in below Tasks section (hint: most recent first).
For that you need to clone <https://github.com/nsauzede/lore.git> and checkout branch `f-drive`
(be smart and remove any local/previous `lore` destination path!).

## 3. Check

Nothing to do for now.

## 4. Update project

Then I want you to update the present HANDOFF.md + any other relevant documents to reflect the current state of the project,
for your future self to take over new tasks I'll append here (mark as done those which are to keep this HANDOFF clean & maintainable).
The step 1 (eg: Rust, maybe Sveltekit) shall always be instructed because of peculiar AI's sandbox constraints wrt Rust.
Eg: if you face any crates version issue in Cargo.toml, please find a working crate set and update it too. Use "=x.y" if required etc.
Add all your work (`cargo clean` !!) in a new git commit (use your identity !) then `git gc` and create a ZIP archive of the whole project + git history and present
it as downloadble zip archive file.
Don't hesitate to enhance this HANDOFF.md if need be.

# Tasks

- [ ] **Build + smoke-test the new write API** — the write endpoints below were
      added *without building* (per instruction). Before (or alongside) the
      frontend task, verify them:
      1. Setup per RUST.md + `apt-get install -y protobuf-compiler` (tonic/prost need `protoc`).
      2. `cargo build -p lore-drive` — new deps `zip` (pinned `=2.2.2`) and
         `tempfile` were added to the root workspace + lore-drive manifests and
         axum gained the `multipart` feature; the first build will update
         `Cargo.lock` (crates.io is reachable from the sandbox). If `zip =2.2.2`
         misbehaves under edition-2024/workspace lints, pick another 2.x and pin it.
      3. Point the binary at a real lore workspace **with a commit identity
         configured** (commits fail with `MissingIdentity` otherwise) and verify:
         - `curl http://localhost:8080/api/v1/download/<file id>` matches `lore` content (b3 hash!)
         - `curl http://localhost:8080/api/v1/download/0 -o root.zip` yields a valid ZIP
         - `curl -X POST -H 'content-type: application/json' -d '{"parent_id":0,"name":"newdir"}' http://localhost:8080/api/v1/mkdir`
           (watch the empty-dir `.lorekeep` fallback path)
         - `curl -F 'file=@x.txt' 'http://localhost:8080/api/v1/upload?parent_id=0'` then re-upload
           without/with `overwrite=true` to see the `409 {conflicts}` / replace flow
         - `curl -X PATCH -d '{"name":"y.txt"}' .../api/v1/node/<id>` and `curl -X DELETE .../api/v1/node/<id>`
         - after each mutation `GET /api/v1/info` must show the new revision, and
           a concurrent `lore` CLI must still work (long-lived context stays ReadOnly;
           write verbs take their own per-call token)
      4. Open questions to resolve while testing (documented in REST_API.md):
         does `load_current_anchor` on the long-lived context observe the new
         anchor after commit (else re-`load_and_connect` in `refresh_tree`)?
         does staging a bare directory commit anything (else `.lorekeep` kicks in)?

- [ ] **Implement the SvelteKit 5 frontend** (postponed from the previous task —
      the REST API had to be augmented first, which is now done; see REST_API.md).
      Create it under `lore-drive/frontend/` (Vite + SvelteKit 5, runes). Dev
      server proxies/points at `http://localhost:8080`; CORS is already permissive.
      Requirements (from the project owner):
      - Tree of folders/files, current path "/" (project root) by default,
        breadcrumb navigation; "enter" a folder (📁 icon) by clicking it
        → `GET /api/v1/tree?node_id=`, `GET /api/v1/info`
      - file/folder card shows: name, uuid/addr etc; file card additionally
        shows size and contents b3 hash — display `address` (`<b3-hash>-<file-id>`)
        and `node_id` exactly as returned, 1-to-1 with the CAS
      - burger menu (⋮) on the right of each card: rename / delete / download
        → `PATCH /api/v1/node/{id}`, `DELETE /api/v1/node/{id}`, `GET /api/v1/download/{id}`
      - "create folder" and "upload" buttons → `POST /api/v1/mkdir`,
        `POST /api/v1/upload?parent_id=` (multipart, part filename may carry
        relative paths for folder upload)
      - main page area is a drag'n'drop zone for files *and* folders (use
        `webkitGetAsEntry`/`FileSystemEntry` traversal to build relative paths)
      - "download" on a folder downloads a ZIP (the backend already serves it)
      - on `409 {error, conflicts:[...]}` from upload, show a **replace / all /
        abort** modal: "all" re-sends everything with `overwrite=true`,
        "replace" re-sends only the selected conflicting files with
        `overwrite=true` (plus the non-conflicting ones without it), "abort" cancels
      - after every successful mutation, refresh the current listing (responses
        carry the new `revision`; stale `node_id`s must not be reused)
      - treat `.lorekeep` entries as hidden
      Keep it minimal & clean; no component library needed. Building the Rust
      backend is not required to *write* the frontend, but end-to-end testing
      needs the task above done.

- [x] **Augment REST API + backend with write & download endpoints** — outcome of
      the "simple SvelteKit UI" task: the UI needs rename/delete/download/upload/
      mkdir which the read-only API lacked, so (per its own instruction) the API
      was augmented first and the frontend postponed (see the two tasks above).
      Backend was updated **without building** (too early — see verify task).
      - `REST_API.md`: new authoritative spec sections — write model
        (workspace-mediated: fs change → `lore::file::stage`/`stage_move` →
        `lore::revision::commit`, one commit per request, global writer mutex,
        tree handle reloaded after each commit), `GET /download/{id}` (file bytes
        or folder ZIP fetched from the CAS so bytes match the displayed b3 hash),
        `POST /mkdir`, `POST /upload` (multipart, relative-path filenames,
        `409 {conflicts}` protocol for the replace/all/abort modal),
        `PATCH /node/{id}` (rename/move via `stage_move`, preserves file_id),
        `DELETE /node/{id}`.
      - Why workspace-mediated: the low-level `lore_revision_tree_{add,delete,
        move,modify,commit}` verbs are **argument-struct stubs only** (no
        implementation in the `lore` crate yet), so writes reuse the proven
        CLI-equivalent high-level flow; implementing the low-level verbs stays
        a possible future task.
      - `lore-drive/src/main.rs`: `AppState` gained `RwLock<TreeState>` (tree
        handle + revision, swapped by `refresh_tree` after each commit, old
        handle closed) and a `write_gate` mutex; helpers `fetch_node_info/
        fetch_node_path/fetch_children/try_resolve_path/cas_fetch_to_file/
        stage_paths/stage_move_path/commit_staged/with_lore_ctx`; handlers for
        download (ZIP via `zip` crate in `spawn_blocking`), mkdir (`.lorekeep`
        fallback for empty dirs), upload (temp-dir buffering, conflict check
        *before* touching the workspace, rollback on failure), patch, delete.
        Repo context stays `ReadOnly`; `stage`/`commit` verbs take their own
        per-call write token (keeps concurrent `lore` CLI usable).
      - Manifests: root `Cargo.toml` gained `tempfile = "3.27.0"` and
        `zip = "=2.2.2"` (default-features off, `deflate`); `lore-drive/Cargo.toml`
        now uses `axum` with the `multipart` feature plus `tempfile`/`zip`.
        `Cargo.lock` intentionally untouched (updated by the first build).

- [x] **Attempt a first build + smoke-test** — Now that the `LORE_CONTEXT` panic is
      fixed, set up a full build environment (see RUST.md + install `protoc` since
      lore-base/lore-revision pull in tonic/prost which need the protobuf compiler),
      then run `cargo build -p lore-drive` and address any remaining compile errors.
      If `protoc` is not available via apt (`apt-get install -y protobuf-compiler`),
      document the blocker and leave build notes here.
      After a successful build, point the binary at a real lore workspace and verify:
      - `curl http://localhost:8080/api/v1/info` returns JSON without panicking.
      - `curl http://localhost:8080/api/v1/tree` returns the root listing.
      - `curl http://localhost:8080/api/v1/node/1` returns a valid node record.

- [x] **Fix startup panic** (`cannot access a task-local storage value without
      setting it first`) — Root cause: `load_and_connect` (and every lore verb)
      calls `execution_context()` which reads from a tokio task-local
      (`LORE_CONTEXT`). The previous `main.rs` called `load_and_connect` directly
      in the async main body with no task-local set.
      **Fix applied in `lore-drive/src/main.rs`:**
      - Extracted the entire workspace-open sequence into a new
        `open_workspace(workdir)` async fn.
      - Before calling it, create a startup `ExecutionContext` with
        `EventDispatcher::no_dispatch()` (no callback needed for init).
      - Wrap the call in `LORE_CONTEXT.scope(startup_ctx, open_workspace(...))`.
      - Added imports: `lore_base::runtime::LORE_CONTEXT`,
        `lore_revision::interface::ExecutionContext`,
        `lore_revision::relay::EventDispatcher`.
      - Per-request handlers are unaffected — they already get their own
        `LORE_CONTEXT` scope via the internal `revision_tree_call`/`storage_call`
        dispatch helpers inside the lore crate.

- [x] **Scaffold `lore-drive` app** — Create `lore-drive/src/main.rs` (and any
      needed `src/*.rs` modules) implementing the three endpoints specified in
      `REST_API.md`.  The implementation requirements are:

      **Bootstrapping**
      - Binary entry-point: `lore-drive/src/main.rs`
      - Bind on `0.0.0.0:8080` by default; accept an optional `--port` CLI arg.
      - Use `#[tokio::main]` with the multi-thread runtime.
      - Emit structured logs via `tracing_subscriber` (env-filter, JSON optional).

      **State**
      - At startup, open the lore workspace from the current working directory.
        Use `lore::repository::RepositoryContext` (or the equivalent high-level
        `lore` crate API) to load the repo, detect the active branch and its
        latest committed revision hash, and build a `LoreRevisionTree` handle
        (via `lore::revision_tree::load::load`).
      - Wrap the loaded state in `Arc<AppState>` and inject it via `axum::Extension`.

      **Endpoints** (see REST_API.md for full shapes):
      - `GET /api/v1/info`  → JSON workspace metadata (repo id, branch id/name, revision, workdir).
      - `GET /api/v1/tree?node_id=<u64>` → JSON listing of a directory's direct children;
        default to ROOT_NODE when `node_id` is absent or 0;
        use `lore::revision_tree::list_children::list_children`.
      - `GET /api/v1/node/:node_id` → JSON node record including path from root;
        use `lore::revision_tree::node_info::node_info` and
        `lore::revision_tree::node_path::node_path`.

      **CORS**
      - Attach `tower_http::cors::CorsLayer::permissive()` so the future SvelteKit
        frontend on a different port can reach the API in development.

- [x] **Design REST_API.md** — Produced `lore-drive/REST_API.md`: a rigorous
      specification of the three read-only endpoints (`GET /api/v1/info`,
      `GET /api/v1/tree`, `GET /api/v1/node/:node_id`) that expose the workspace's
      revision tree.  Identity types (Hash/Context/Partition/Address/NodeID)
      are mapped 1-to-1 from lore-base to JSON with no re-encoding.
      Also: updated `lore-drive/Cargo.toml` with real dependencies (axum, tokio,
      tower-http, serde, serde_json, lore-base, tracing) and added `lore-drive`
      to the root workspace `Cargo.toml` members + workspace.dependencies.
