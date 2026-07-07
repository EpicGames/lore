# HANDOFF

## 1. Setup

Firstly, apply RUST.md to correctly setup the required Rust toolchain
(the workspace uses edition 2024 which requires Rust ≥ 1.85), and install
`protobuf-compiler` (tonic/prost need `protoc`).

**Sandbox build survival guide** (learned the hard way across sessions):
- Check free disk *first* (`df -h /`): a full debug build with default
  settings needs ~10 GB.  Free space by deleting `target/debug/incremental`
  and stale caches (`~/.cache/uv`, `~/.cache/puppeteer` were ~2 GB).
- Build with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0` — debug info
  roughly triples artifact size and compile time here, and single big crates
  (`lore`, `lore-revision`) otherwise exceed one tool-call time budget.
- Tool calls are capped around ~4½ minutes and background processes die
  between calls: run `timeout 250 cargo build -p <crate>` repeatedly —
  cargo resumes from cached artifacts, ~3 chunks for a cold workspace build
  (same for `-p lore-client`, which provides the `lore` CLI binary).
- Long-running servers survive within a *message* via
  `(setsid <cmd> > log 2>&1 < /dev/null &)` but are killed between user
  messages — restart them per message and re-check `pgrep`.
- **`pkill -f` trap**: the pattern matches the *current* `/bin/sh -c ...`
  command line too (it contains the pattern text), killing your own shell —
  the tool call then reports returncode −1 with no output and *no side
  effects*.  Kill servers by port instead: `fuser -k 8080/tcp 5173/tcp`
  (needs `apt-get install -y psmisc`), or `pkill -x <exact-binary-name>`.
- **Browser E2E works**: Playwright (npm, dev-dependency) driving the
  preinstalled system Chrome at `/opt/google/chrome/chrome` with
  `{ executablePath, args: ['--no-sandbox','--disable-dev-shm-usage'] }`.
  Do NOT `npx playwright install` (download hosts are blocked) — and don't
  delete `~/.cache/puppeteer` if you plan to use puppeteer instead.
  `/opt/pw-browsers` also has Playwright-layout chromium builds, but their
  revision rarely matches the npm playwright version — `executablePath` to
  system Chrome is the reliable route.

For the frontend: Node/npm are preinstalled; `npm install` + `npm run build`
inside `lore-drive/frontend/` just work (registry is whitelisted).

## 2. Tasks (see below)

Then I want you to perform the tasks not yet done in below Tasks section (hint: most recent first).
For that you need to clone <https://github.com/nsauzede/lore.git> and checkout branch `f-drive`
(be smart and remove any local/previous `lore` destination path!).

## 3. Check

Quick health check of the current state, in a scratch workspace
(`lore repository create --offline <name>` inside an empty dir):

```bash
target/debug/lore-drive &          # drive mode (add --ui lore-drive/frontend/build to also serve the SPA)
curl -s localhost:8080/api/v1/info # → {"mode":"drive", ...}
curl -F "file=@somefile" "localhost:8080/api/v1/upload?parent_id=0"  # → 201, non-zero b3 address
# replace-upload must refresh BOTH content and metadata (the fixed stale-size bug):
curl -F "file=@bigger_somefile;filename=somefile" "localhost:8080/api/v1/upload?parent_id=0&overwrite=true"
curl -s localhost:8080/api/v1/tree  # → the file's size and address are the NEW ones
```

Full browser E2E (29 checks): build the frontend, start lore-drive in a
scratch workspace, then `node frontend/e2e.mjs` (against `npm run dev` on
:5173) or `E2E_BASE=http://localhost:8080 node frontend/e2e.mjs` (against
`lore-drive --ui frontend/build`).  See `frontend/README.md`.

## 4. Update project

Then I want you to update the present HANDOFF.md + any other relevant documents to reflect the current state of the project,
for your future self to take over new tasks I'll append here (mark as done those which are to keep this HANDOFF clean & maintainable).
The step 1 (eg: Rust, maybe Sveltekit) shall always be instructed because of peculiar AI's sandbox constraints wrt Rust.
Eg: if you face any crates version issue in Cargo.toml, please find a working crate set and update it too.
Add all your work (`cargo clean` !!) in a new git commit (use your identity !) then `git gc` and create a ZIP archive of the whole project + git history and present
it as downloadble zip archive file.
Don't hesitate to enhance this HANDOFF.md if need be.

# Tasks

- [ ] Discovered a bug:
      1. upload nested folder1/file1
      2. delete folder1/
      3. it "works" as in: browser shows empty root, and on-disk FS shows an empty lore reop (remains only .lore)
      4. HOWEVER: trying to upload again nested folder1/file1, shows the popup "replace?"
      Please fix/test that.

- [ ] Let's talk about /metadata/
      What if I'd like for the browser user, to set/list custom metadata per browser item (folder/file), possible ?
      Eg: in the "..." menu of those, add a "Properties" submenu that lists those set user-properties, and let add
      new ones. Let's start simple: such a property is key/value (string/string)
      Can we elegantly stuff that with minimal effort, eg: inside the existing "mutable store", for cheap ?
      Can we then add a global "search" toolbar, letting us search for folder/file names, and/or property key/value ?
      Could this be elegant/minimalist/fast ? Can that be unit(cargo)/integration(hurl)/e2e(playwright) tested ?
      If complex, this task could result in drafting spec for further task(s)

- [x] **End-to-end test the frontend against the backend in a browser-like
      environment** — done this session with Playwright driving the system
      Chrome (see survival guide).  `frontend/e2e.mjs` (29 checks, all green,
      run against BOTH the dev-proxy topology on :5173 and the new
      single-server topology on :8080): navigation + breadcrumbs, file upload
      via picker, **folder upload via the `webkitdirectory` picker** (relative
      paths preserved: `bundle/sub/two.txt` lands nested), drag'n'drop
      (synthetic `DataTransfer` exercises the `getAsFile` fallback; real
      `webkitGetAsEntry` folder-drop can't be synthesized headlessly — the
      traversal's output shape is identical to the directory picker's, which
      IS covered), the 409 modal: abort / replace-all / **replace-selected
      with `conflictPathOf` matching verified inside a subfolder** (backend
      reports `/docs/bundle/one.txt`; non-conflicting files in the same batch
      still upload; deselecting every conflict disables "Replace selected" by
      design — abort leaves everything untouched), rename (file_id preserved),
      delete (confirm accepted and dismissed), file download (bytes verified),
      folder download (ZIP magic verified), `.lorekeep` hidden (planted one
      via the API into an empty folder; UI keeps showing "empty").
      **MY REMARKS — the stale-size bug: reproduced, root-caused, fixed.**
      - Repro (curl or UI): upload `folder1/file1` (4 B), replace-upload the
        same path with 12 B → workdir, CAS and `/download` all served the new
        bytes, but `/tree`/`/node` kept **size 4** and the staged revision
        never changed, so `refresh_tree` had nothing to swap.
      - Root cause (upstream, `lore-revision/src/stage.rs`,
        `stage_node_from_metadata`): a node that is **already staged is
        skipped wholesale** (`if !node.is_staged() || force …`).  In drive
        mode nothing ever commits, every node stays `StagedAdd` forever, so
        re-staging never refreshed the recorded size/mode.  The address only
        *looked* fresh because lore-drive's CAS-put epilogue updates its
        address cache independently of the tree.
      - Fix (in `stage.rs`): for an already-staged, non-deleted **file** whose
        filesystem size or mode disagrees with the staged record, rewrite the
        node record, mark block+state dirty and report a modification — the
        stage driver then re-serializes the staged revision, `FileStageRevision`
        fires, and lore-drive swaps in a fresh tree.  Guarded to not run when
        the normal staging block below will handle the node anyway (`force`,
        merge flags), and to never resurrect a staged-delete tombstone.
      - Regression-verified: identical re-upload is still a revision-
        preserving no-op; same-size same-mode content change still updates
        bytes + address while metadata legitimately stays (documented in
        REST_API.md "Verified behavior & caveats"); mkdir / rename / delete /
        `.lorekeep` / dedup unaffected.
      **Polish done**: `lore-drive --ui <dir>` now serves the built SPA
      (adapter-static, `index.html` fallback via `tower_http::services::
      ServeDir`/`ServeFile`, `fs` feature added to the workspace `tower-http`)
      on every non-`/api` route — one server, no dev proxy.  E2E suite passes
      against it directly.

- [x] **Build + smoke-test the write API (drive mode default + `--versioned`)**
      — done this session; every checklist item verified against a real
      workspace, four bugs found, all fixed:
      1. *(fixed)* A staging **no-op returned 500** ("stage emitted no staged
         revision").  Bites on byte-identical re-uploads *and* on any
         **same-size, same-mode content change** (staged records carry no
         content hash, so such an edit yields a bit-identical staged
         revision).  `stage_paths` now returns `Option<Hash>` (`None` =
         success no-op) and the upload epilogue still CAS-puts the new bytes
         → address stays 1-to-1 with content even when the change-tag can't
         move.  mkdir's empty-dir detection was adapted (`None` ⇒ `.lorekeep`
         fallback).
      2. *(fixed)* **Upload rollback destroyed data**: on failure it deleted
         every placed file, including *pre-existing* files being overwritten
         (observed live: it deleted the workspace's `notes.txt`).  Overwritten
         files are now backed up to the request tempdir first and restored on
         rollback.
      3. *(fixed, in the `lore` crate)* **Staged deletions leaked into
         listings**: `list_children` streamed tombstoned (staged-delete)
         nodes and its child event carries no deletion flag, so a deleted
         file remained visible in `/tree` forever (drive mode never commits
         the deletion away).  `lore/src/revision_tree/list_children.rs` now
         skips `is_staged_delete()` children.
      4. *(fixed)* Versioned-mode no-op commit: the `NothingStaged` outcome
         emits **no error event** — only `Complete` status **21** (its
         discriminant in the `CommitError` error-set).  `commit_staged`
         now matches the status code (`COMMIT_STATUS_NOTHING_STAGED`), with
         the old message matcher kept as belt-and-braces.
      Answers to the open questions (also folded into REST_API.md):
      - staging alone requires **no identity** (drive mode runs identity-less;
        `lore auth info` failing offline probes the auth *endpoint*, unrelated);
      - `served_revision` observes staged-anchor/anchor updates fine through
        `refresh_tree` after each mutation (tree swap verified);
      - `load` accepts a staged revision hash (drive mode serves one across
        restarts — persistence verified);
      - the `.lorekeep` fallback triggers for empty dirs (mkdir path verified
        in drive mode);
      - the owner's `CAS miss … served from workdir` warning was **not
        reproducible** with the fixed build (uploaded, CLI-staged and
        committed files all download cleanly with matching b3 hashes); it
        indicates a blob genuinely absent from the local store and the
        workdir fallback is the designed safety net — documented in
        REST_API.md.
      Also verified: mkdir/rename/move/delete; 409 `{error, conflicts}`;
      dedup (store size unchanged on identical re-upload); one node/file_id
      across 3 re-uploads of the same path; drive↔CLI coexistence is
      **sequential only** — a running lore-drive holds `.lore/lock`, so
      concurrent `lore` CLI commands block until it exits; commit identity =
      top-level `identity = "Name <mail>"` in `.lore/config.toml`.

- [x] **Implement the SvelteKit 5 frontend** — done this session under
      `lore-drive/frontend/` (SvelteKit 2 + Svelte 5 runes + Vite 6,
      adapter-static SPA, no component library).  Single-page app
      (`src/routes/+page.svelte`) + tiny API client (`src/lib/api.js`):
      breadcrumb tree navigation, cards with exact `node_id` and verbatim
      `address` (`<b3-hash>-<file-id>`), size for files, a deterministic
      color "content fingerprint" derived from the b3 hash (visual 1-to-1
      restatement of the CAS mapping), ⋮ menu (rename/download/delete —
      folder download hits the ZIP endpoint), create-folder + upload-files +
      upload-folder buttons, whole-page drag'n'drop with
      `webkitGetAsEntry`/`FileSystemEntry` traversal for folders, 409
      replace-selected/all/abort modal, refresh-after-mutation treating
      `revision` as an ETag, `.lorekeep` hidden.  `npm run build` passes;
      dev-server proxy (`/api` → :8080) verified with curl against the live
      backend.  See `frontend/README.md`.  Browser-level e2e remains (task
      above).

- [x] **Single-version drive semantics (owner request): stage-only drive mode
      (default) + `--versioned`, and zero-hash fixes** — the owner asked for a
      no-versioning "USB-stick" drive (one copy per path, no history), or
      alternatively an eternal-amend scheme. Feasibility (verified in code):
      - *eternal amend*: not a primitive — `lore revision amend`
        (`lore-revision/src/revision/amend.rs`) only rewrites the tip's
        message/metadata; it never folds staged content, and revision parent
        pointers cannot be rewritten via any API → rejected.
      - *stage-only*: viable **only** with lore-drive populating the CAS
        itself, because content hashing + immutable-store writes are a
        **commit**-time step (`lore-revision/src/commit.rs`:
        `write_from_file_with_tracker` per file, blake3 rehash per dir;
        staged nodes carry a ZERO content hash — `stage_node_from_metadata`
        records only file_id/size/mode). The owner's on-disk rewrite of
        main.rs (drive mode default + `--versioned` + `cas_put_file` on
        upload) adopted exactly that; this session kept it and fixed the
        three latent zero-hash bugs it inherited:
        1. `/tree`+`/node` would display `0000…-<file_id>` for staged files →
           new `effective_address` materializer: idempotent `cas_put_file` of
           the workdir bytes at the node's file_id, cached in `addr_cache`
           (cleared on every tree swap); zero-SIZE files keep the zero hash
           (lore's empty-content convention); link-repo children pass through.
        2. downloads of staged files returned 0 bytes — `get_file` with a
           zero hash *truncates-to-empty and succeeds*, so the workdir
           fallback never fired → `read_file_bytes` short-circuits zero-hash
           reads to the workdir; single-file download materializes first.
        3. upload epilogue's `stored == node.address` always warned (staged
           side is zero) → compares file_id context only; CAS-returned
           address feeds the response + cache.
        Plus: `commit_staged` → `Result<Option<Hash>>` with `is_nothing_staged`
        so identical re-uploads in `--versioned` are success no-ops too.
      - REST_API.md rewritten accordingly: dual-mode overview, "Single-version
        drive semantics" section (guarantees, drive-mode subtlety, feasibility
        notes, client consequences as ETag semantics), download/upload/mkdir/
        info docs updated, implementer note on the materialization cache.

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
