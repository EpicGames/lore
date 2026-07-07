# lore-drive frontend

Minimal SvelteKit 5 (runes) single-page UI for the `lore-drive` REST backend
(see `../REST_API.md`).

## Run (development)

```bash
# 1. start the backend inside a lore workspace
cd /path/to/workspace && lore-drive            # or: lore-drive --versioned

# 2. start the frontend dev server (proxies /api → http://localhost:8080)
cd lore-drive/frontend
npm install
npm run dev                                    # → http://localhost:5173
```

Point the app at a different backend with `VITE_API_BASE=http://host:port`.

## Build

`npm run build` produces a static SPA in `build/` (adapter-static, fallback
`index.html`). Any static file server can host it; the backend stays a pure
REST API with permissive CORS.

## Run (single server, no dev proxy)

```bash
npm run build
cd /path/to/workspace && lore-drive --ui /path/to/lore/lore-drive/frontend/build
# → http://localhost:8080 serves both the SPA and /api/v1/*
```

## End-to-end tests (real Chromium)

`e2e.mjs` drives the full UI with Playwright: navigation, file/folder
upload (including the `webkitdirectory` picker and drag'n'drop), the 409
abort / replace-all / replace-selected modal (with absolute-virtual-path
matching after navigating into a subfolder), rename, delete (confirm
dialog), file download (bytes verified) and folder download (ZIP magic
verified), `.lorekeep` hiding, and the replace-upload metadata-refresh
scenario (size must update along with content and address).

```bash
npm install                    # pulls playwright (dev dependency)
# start the backend in a scratch workspace, plus either `npm run dev`
# (test via :5173) or `lore-drive --ui frontend/build` (test via :8080):
node e2e.mjs                                   # defaults to :5173
E2E_BASE=http://localhost:8080 node e2e.mjs    # single-server mode
# E2E_CHROME=/path/to/chrome to override the browser binary
```

All 29 checks pass in both topologies (Chromium 141).

## What it does

- Browsable tree, breadcrumb navigation, click a 📁 to enter it
- Cards show name + `node_id`; files additionally show size and the
  `address` (`<b3-hash>-<file-id>`) **verbatim** — 1-to-1 with the CAS.
  The colored swatch is a *content fingerprint* derived from the b3 hash.
- ⋮ menu: rename, download (folders arrive as ZIP), delete
- "Create folder" / "Upload files" / "Upload folder" buttons
- The whole page is a drag'n'drop zone for files **and** folders
  (`webkitGetAsEntry` traversal preserves relative paths)
- On `409 {conflicts}` a replace-selected / replace-all / abort modal appears
- After every mutation the listing refreshes; the `revision` change-tag is
  treated as an ETag (it may stay unchanged on idempotent no-ops) and stale
  `node_id`s are never reused
- `.lorekeep` placeholder entries are hidden
