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
