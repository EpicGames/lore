# lore-drive REST API

This document is the authoritative specification for the HTTP API served by the
`lore-drive` binary.

---

## Overview

`lore-drive` is a thin Axum/Tokio HTTP backend that wraps the `lore` client
library.  It is started inside a lore workspace (the same working directory
where you would invoke the `lore` CLI) and exposes the workspace's revision
tree over a local REST API so a browser-based frontend can display a browsable
file/folder tree.

The API has two halves:

- a **read API** (`info`, `tree`, `node`, `download`) that serves the
  *current* tree and file contents straight from the CAS, and
- a **write API** (`upload`, `mkdir`, `PATCH node`, `DELETE node`) whose
  mutations are **workspace-mediated**: every mutation is performed as a
  filesystem change inside the working directory, then staged
  (`lore::file::stage` / `stage_move`) — and, in versioned mode only, also
  committed (`lore::revision::commit`) — the same flows the `lore` CLI uses.

**lore-drive is a single-version drive by default** ("dumb cloud drive" /
USB-stick semantics): the API exposes exactly one version of the tree — the
current one — no history browsing, no version selection.  Two serving modes:

- **Drive mode (default)** — mutations update the working directory and the
  single **staged revision** (`lore::file::stage`/`stage_move`), **never
  committing**.  Every mutation replaces the one staged snapshot in place.
  Content is pushed into the CAS by lore-drive itself (`lore_storage_put_file`),
  so files still have node ids, `file_id`s and real BLAKE3 content addresses,
  and populate the mutable/immutable stores — deduplicated: uploading the same
  path/content N times keeps exactly one stored copy.
- **Versioned mode (`--versioned`)** — every *effective* mutating request
  produces exactly one commit on the active branch.

See *Single-version drive semantics* below for what this means precisely and
for the feasibility analysis behind the design.

**Base URL**: `http://localhost:8080`  
**Protocol**: HTTP/1.1, JSON bodies (`Content-Type: application/json`).  
**Error shape** (all 4xx / 5xx responses):

```json
{ "error": "<human-readable message>" }
```

---

## Identity types

These are the lore-internal identifiers exposed verbatim in every response so
the frontend shows exactly what is stored in the CAS — no translation, no
re-encoding.

| Name | Rust type | JSON representation | Description |
|------|-----------|---------------------|-------------|
| `Hash` | `lore_base::types::Hash` | 64-char lowercase hex string | 256-bit BLAKE3 content hash |
| `Context` / `BranchId` | `lore_base::types::Context` | 32-char lowercase hex string | 128-bit opaque context / branch identifier |
| `Partition` / `RepositoryId` | `lore_base::types::Partition` | 32-char lowercase hex string | 128-bit opaque partition / repository identifier |
| `Address` | `lore_base::types::Address` | `"<64-hex>-<32-hex>"` | Content hash paired with a context |
| `NodeID` | `lore_revision::node::NodeID` | unsigned 64-bit integer | Opaque node identifier within a revision tree |

---

## Endpoints

### `GET /api/v1/info`

Returns metadata about the workspace open in this `lore-drive` instance.

#### Response `200 OK`

```json
{
  "repository_id": "<32-char hex>",
  "branch_id":     "<32-char hex>",
  "branch_name":   "main",
  "revision":      "<64-char hex>",
  "workdir":       "/absolute/path/to/workspace",
  "mode":          "drive"
}
```

| Field | Description |
|-------|-------------|
| `repository_id` | `Partition` — repository UUID as stored in the CAS |
| `branch_id` | `Context` — branch UUID as stored in the CAS |
| `branch_name` | Human-readable branch name |
| `revision` | `Hash` — current change-tag (served revision; see *Single-version drive semantics*) |
| `workdir` | Absolute filesystem path of the workspace root |
| `mode` | `"drive"` (stage-only, no history — default) or `"versioned"` (commit per mutation) |

---

### `GET /api/v1/tree`

List the direct children of a directory node.

#### Query parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `node_id` | `u64` | root node | Node ID of the directory to list (omit or pass `0` for the repository root) |

#### Response `200 OK`

```json
{
  "repository_id": "<32-char hex>",
  "revision":      "<64-char hex>",
  "node_id":       42,
  "children": [
    {
      "node_id":  43,
      "name":     "src",
      "kind":     "directory",
      "mode":     493,
      "size":     0,
      "address":  null
    },
    {
      "node_id":  44,
      "name":     "README.md",
      "kind":     "file",
      "mode":     420,
      "size":     2048,
      "address":  "abcdef0123...64hex...-fedcba9876...32hex..."
    },
    {
      "node_id":  45,
      "name":     "vendor",
      "kind":     "link",
      "mode":     0,
      "size":     0,
      "address":  "1111...64hex...-2222...32hex..."
    }
  ]
}
```

| Field | Description |
|-------|-------------|
| `repository_id` | `Partition` of the repository that owns the listed directory (may differ from the workspace repo when listing through a link) |
| `revision` | `Hash` — revision the listing belongs to |
| `node_id` | The resolved node ID that was listed (equals the input `node_id`, or root when omitted) |
| `children[].node_id` | Opaque `NodeID` |
| `children[].name` | Entry name within its parent |
| `children[].kind` | One of `"file"`, `"directory"`, `"link"` |
| `children[].mode` | Unix permission bits (decimal integer) |
| `children[].size` | Byte size as stored in the CAS; `0` for directories and links |
| `children[].address` | `Address` string (`"<hash>-<context>"`) for files; `null` for directories; the link target address for links |

#### Error responses

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | `node_id` is not a valid directory node (leaf, link that resolves to a leaf, unknown ID) |
| `500 Internal Server Error` | Storage or I/O failure during iteration |

---

### `GET /api/v1/node/:node_id`

Fetch the full metadata record for a single node.

#### Path parameter

| Parameter | Description |
|-----------|-------------|
| `node_id` | `u64` — node ID to query |

#### Response `200 OK`

```json
{
  "node_id":  44,
  "parent_id": 42,
  "name":     "README.md",
  "kind":     "file",
  "mode":     420,
  "size":     2048,
  "address":  "abcdef...64hex...-fedcba...32hex...",
  "path":     "/src/README.md"
}
```

| Field | Description |
|-------|-------------|
| `node_id` | The queried node |
| `parent_id` | Parent node ID (`0` for root) |
| `name` | Entry name within its parent |
| `kind` | `"file"`, `"directory"`, or `"link"` |
| `mode` | Unix permission bits |
| `size` | Byte size; `0` for non-files |
| `address` | `Address` string for files/links; `null` for directories |
| `path` | Slash-separated path from root to this node, always starting with `/` |

#### Error responses

| Status | Condition |
|--------|-----------|
| `404 Not Found` | `node_id` is unknown or the root sentinel `0` is queried and the tree is empty |
| `400 Bad Request` | `node_id` cannot be parsed as a `u64` |

---

## Single-version drive semantics

The project owner's requirement: behave like a dumb cloud drive / USB stick —
files and folders are "in the system" (node ids, `file_id` UUIDs, BLAKE3
content hashes, mutable/immutable stores populated) but there is only ever
**one** version of the tree.  Uploading the same file/path three times must
keep exactly one stored copy.

### What the API guarantees (both modes)

1. **One visible version.**  Every endpoint serves the *current* tree — the
   staged revision in drive mode (falling back to the committed anchor when
   nothing is staged yet), the branch tip in versioned mode.  There is no
   endpoint to list, browse, or check out history.  The `revision` field in
   responses is an **opaque change-tag** (think HTTP ETag): compare it to
   know whether anything changed; do not interpret it as a navigable version.
2. **One stored copy per content.**  The store is content-addressed: storing
   the same bytes N times writes the fragments **once** (deduplicated).
   Re-uploading an identical file is a no-op at the storage layer by
   construction.
3. **One identity per path.**  Overwriting an existing file reuses its node —
   the `file_id` (the context half of the `Address`) is preserved, so a path
   keeps a single stable identity across content updates.
4. **Idempotent writes.**  A mutation that changes nothing (e.g. re-uploading
   identical content with `overwrite=true`) is a **success no-op**: the
   change-tag stays the same, no error is returned.  Drive mode detects this
   as an unchanged staged-revision hash; versioned mode maps the *nothing
   staged* commit outcome to success.

### How drive mode works (and the one subtlety)

Drive mode never commits.  Mutations are: filesystem change → `stage`/
`stage_move` → serve the resulting **staged revision** (a real serialized
revision state, loadable by the same revision-tree machinery as a commit).
Each stage *replaces* the staged snapshot in place — no history accrues, not
even invisible metadata.  An external `lore` CLI sees the accumulated staged
changes and may commit them at any time; lore-drive keeps working either way.

The subtlety: **staged file nodes carry a zero content hash.**  In lore,
content hashing and immutable-store writes are performed by the *commit* step
(`lore-revision/src/commit.rs`: `write_from_file_with_tracker` per file,
blake3 rehash per directory; see also `stage_node_from_metadata` in
`stage.rs`, which records `file_id`, size and mode but no content hash).
Drive mode compensates at the lore-drive layer:

- **On upload**, each file's bytes are pushed into the CAS at the staged
  node's `file_id` context via `lore_storage_put_file` (content-addressed →
  dedup for free).  The returned address is the authoritative one.
- **On read** (`/tree`, `/node`, `/download`), a file node whose record still
  has a zero hash gets its address **materialized**: the workdir bytes are
  `put` into the CAS (idempotent) and the resulting address is served and
  cached for the current change-tag.  This keeps the displayed b3 hash
  1-to-1 with the CAS and also covers files staged by an external CLI.
  Zero-*size* files legitimately keep the zero hash (lore's empty-content
  convention).
- **Downloads** never send a zero hash to the CAS (`get_file`'s contract for
  a zero hash is *truncate-to-empty and succeed*); such reads are served from
  the working directory instead.

Directory nodes have no materialized hash in drive mode (directory hashes are
also computed at commit); their `address` is `null` as usual.

### Feasibility notes (alternatives considered)

- **Eternal amend** (`git commit --amend --no-edit` equivalent): not
  available as a primitive.  `lore revision amend`
  (`lore_revision::revision::amend::amend_revision`) only rewrites the tip's
  *metadata/message* and re-anchors the branch; it does not fold staged
  content into the tip, and there is no API to rewrite a committed revision's
  parent pointers.  Emulating it would mean hand-editing serialized `State`
  records — fragile, rejected.
- **Commit per mutation** (kept as `--versioned`): the standard flow; history
  accrues (a ~320-byte revision record plus changed node blocks per
  mutation) while file *content* stays deduplicated.  If a middle ground is
  ever wanted (real commits, bounded history), a future task can add periodic
  history squashing / `obliterate` of unreferenced revisions — the drive API
  needs no change for it.

### Consequences for clients

- Treat `revision` as an ETag.  Refresh listings when it changes.
- A `2xx` on a mutation does **not** imply a new `revision` (idempotent
  no-op keeps the old one).
- Node ids remain per-version internally; after any mutation response with a
  *new* `revision`, re-fetch listings and do not reuse stale ids.
- `GET /api/v1/info` reports the serving `mode` (`"drive"` or
  `"versioned"`); clients need no behavioral switch — the contract above is
  identical in both.

---

## Write model

All mutating endpoints share the following contract:

1. **Workspace-mediated.**  The backend performs the change on the real
   working directory (`create_dir`, write bytes, `rename`, `remove_*`),
   then stages the affected paths — and, in versioned mode, commits.  The
   generated commit message (`"lore-drive: <verb> <path>"`) is
   drive-internal noise, never surfaced by the API.
2. **Atomic per request.**  One successful request = one staged-snapshot
   replacement (drive mode) or at most one commit (versioned mode; zero for
   a no-op).  On any error before that point, the backend rolls back its
   filesystem changes (best effort) and the current tree is untouched.
3. **Serialized.**  A process-global writer mutex serializes mutations, so
   two concurrent `POST /upload` requests never interleave stage[/commit].
4. **Fresh tree after effective mutations.**  After an effective stage (or
   commit), the backend reloads its tree handle from the new served
   revision.  Every success response carries the current `revision`
   change-tag.
5. **Identity for commits.**  Versioned mode needs a configured lore
   identity in the workspace (same requirement as `lore revision commit`);
   missing identity surfaces as `500` with the underlying error message.
   Drive mode does not commit and has no such requirement — verify during
   smoke testing whether staging alone also expects one.
6. **No-op detection.**  A *nothing staged* outcome from stage+commit is
   mapped to success with the unchanged `revision` (see single-version
   semantics above).

Uniform error shape stays `{ "error": "<message>" }`; the `409 Conflict`
upload response adds a `conflicts` array (see below).

---

### `GET /api/v1/download/{node_id}`

Download content.  Bytes are fetched from the CAS (`lore_storage_get_file`)
at the node's *effective* address (drive mode materializes zero-hash records
first — see the semantics section), so what you download is exactly the
content whose BLAKE3 hash is displayed in `/tree` / `/node` responses.  When
a file's record still has a zero hash (e.g. staged by an external CLI and not
yet materialized) or the CAS read fails, the bytes are served from the
working directory as a fallback — a zero hash is never sent to the CAS.

| Node kind | Response |
|-----------|----------|
| `file` | `200 OK`, `Content-Type: application/octet-stream`, `Content-Disposition: attachment; filename="<name>"`, raw content bytes |
| `directory` | `200 OK`, `Content-Type: application/zip`, `Content-Disposition: attachment; filename="<name>.zip"` (`root.zip` for the root node), a ZIP archive of the whole subtree |
| `link` | `400 Bad Request` — link download is out of scope for v1 |

ZIP details: entry paths are relative to the downloaded directory, `deflate`
compression, empty directories included as directory entries, `link` children
skipped.  The archive is assembled from the *served* tree (recursive
`list_children` walk) with each file fetched from the CAS (workdir fallback
per file as above).

#### Error responses

| Status | Condition |
|--------|-----------|
| `404 Not Found` | unknown `node_id` |
| `400 Bad Request` | `node_id` unparsable, sentinel, or a link |
| `500 Internal Server Error` | CAS read failure |

> **v1 note**: file/ZIP payloads are buffered in memory before the response
> is sent.  Fine for a dev tool; streaming is a future enhancement.

---

### `POST /api/v1/mkdir`

Create a directory.

#### Request body (`application/json`)

```json
{ "parent_id": 0, "name": "docs" }
```

| Field | Type | Description |
|-------|------|-------------|
| `parent_id` | `u64` | Node id of the parent directory (`0` = root) |
| `name` | string | New directory name — single path component, no `/`, not `.` or `..`, non-empty |

#### Response `201 Created`

```json
{ "node_id": 57, "path": "/docs", "revision": "<64-hex>" }
```

`node_id` may be `null` when the underlying revision does not materialize the
new (empty) directory as a node — see the empty-directory caveat below.

#### Empty-directory caveat

lore stages *files*; a freshly created empty directory may stage to nothing.
The implementation first tries `create_dir` + stage (in drive mode an
unchanged staged-revision hash means nothing was recorded; in versioned mode
the commit reports *nothing staged*); it then drops a `.lorekeep` placeholder
file inside the new directory and stages again.  Frontends should treat
`.lorekeep` as a hidden implementation detail.

#### Error responses

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | invalid `name`, `parent_id` not a directory |
| `404 Not Found` | unknown `parent_id` |
| `409 Conflict` | an entry with that name already exists (tree or filesystem) |

---

### `POST /api/v1/upload?parent_id=<u64>&overwrite=<bool>`

Upload one or more files (optionally nested in folders) into a directory.

#### Query parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `parent_id` | `0` (root) | Directory node the upload lands in |
| `overwrite` | `false` | Replace existing files at colliding paths |

#### Request body (`multipart/form-data`)

One part per file.  The part's `filename` may contain forward-slash-separated
relative path segments (`sub/dir/file.txt`) — this is how folder upload and
drag-and-drop of directories are transported.  Path segments must be plain
components: no leading `/`, no `.`/`..`, no backslashes; violations reject the
whole request with `400`.

#### Conflict semantics ("replace / all / abort" support)

Before touching the workspace, every target path is resolved against the
current (served) tree:

- target exists as a **file** → conflict
- target exists as a **directory** while uploading a file (or vice versa) → conflict
- uploading *into* an existing directory (merging) → **not** a conflict

If any conflict exists and `overwrite=false`, the request is a no-op and
returns:

```json
HTTP 409
{ "error": "3 path(s) already exist", "conflicts": ["/docs/a.txt", "/docs/img/b.png", "/docs/c.md"] }
```

The frontend uses this to drive its *replace / all / abort* modal, then either
aborts, re-sends everything with `overwrite=true` ("all"), or re-sends a
filtered part list ("replace" selected ones only).

#### Response `201 Created`

```json
{
  "revision": "<64-hex>",
  "files": [
    { "name": "a.txt", "path": "/docs/a.txt", "node_id": 61, "size": 123, "address": "<64-hex>-<32-hex>" }
  ]
}
```

`node_id`/`address` are resolved from the current tree (best effort;
`null` if resolution fails).

**Idempotency**: re-uploading byte-identical content over the same paths with
`overwrite=true` is a success no-op — `201` with the *unchanged* `revision`
change-tag, and the content remains stored once (CAS dedup).

In drive mode the response `address` for each file is the authoritative
CAS address returned by the content `put` (the staged node record itself
keeps a zero hash — see the semantics section).

#### Error responses

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | malformed multipart, illegal path segment, `parent_id` not a directory |
| `404 Not Found` | unknown `parent_id` |
| `409 Conflict` | colliding paths with `overwrite=false` (body carries `conflicts`) |
| `413 Payload Too Large` | body exceeds the configured limit (default 1 GiB) |

---

### `PATCH /api/v1/node/{node_id}`

Rename and/or move a node (file or directory).  Preserves the node's
`file_id` history by staging a proper move (`lore::file::stage_move`).

#### Request body (`application/json`)

```json
{ "name": "new-name.txt" }
```
or
```json
{ "parent_id": 42 }
```
or both.  At least one of `name` / `parent_id` must be present.

| Field | Description |
|-------|-------------|
| `name` | New entry name (single component rules as in `mkdir`) |
| `parent_id` | Node id of the destination directory |

#### Response `200 OK`

The updated node record (same shape as `GET /api/v1/node/{id}`) plus the new
revision:

```json
{ "node_id": 61, "parent_id": 42, "name": "new-name.txt", "kind": "file",
  "mode": 420, "size": 123, "address": "<hash>-<ctx>", "path": "/docs/new-name.txt",
  "revision": "<64-hex>" }
```

A no-op rename (same parent, same name) returns `200` with the current record
and the *current* revision — no commit is made.

#### Error responses

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | root node targeted, invalid `name`, destination not a directory, destination inside the moved subtree |
| `404 Not Found` | unknown `node_id` / `parent_id` |
| `409 Conflict` | destination already has an entry with that name |

---

### `DELETE /api/v1/node/{node_id}`

Delete a file or a directory subtree.

#### Response `200 OK`

```json
{ "revision": "<64-hex>" }
```

#### Error responses

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | root node targeted, sentinel/unparsable id |
| `404 Not Found` | unknown `node_id` |
| `500 Internal Server Error` | filesystem or stage/commit failure |

---

## Notes for the implementer

1. **Single repository**.  The process serves exactly one workspace.
   Repository/branch selection at runtime is out of scope.

2. **Link traversal**.  The `GET /api/v1/tree` endpoint transparently resolves
   a link `node_id` to its target directory exactly as
   `lore_revision_tree_list_children` does (bounded by `MAX_LINK_DEPTH`).
   The response `repository_id` and `revision` fields reflect the *resolved*
   target, not the link node itself.  Write endpoints do **not** cross links:
   mutating inside a linked repository is out of scope for v1.

3. **Root sentinel**.  `node_id = 0` (or absent) means the repository root
   (`ROOT_NODE`).  The `INVALID_NODE` sentinel must never be accepted as input.

4. **Address encoding**.  Use the `Address` `Display` impl which produces
   `"<64-hex>-<32-hex>"` and the matching `FromStr` for round-trips.  Expose
   it exactly — do not base64-encode or otherwise transform the bytes.

5. **Hash algorithm**.  Lore uses BLAKE3 throughout.  The `Hash` type wraps a
   32-byte BLAKE3 digest and serialises as a 64-char lowercase hex string.

6. **Why workspace-mediated writes?**  The low-level
   `lore_revision_tree_{add,delete,move,modify,commit}` verbs currently exist
   as argument structs only (no implementation in the `lore` crate), so the
   write path reuses the proven high-level workspace flow instead: filesystem
   change → `lore::file::stage`/`stage_move` [→ `lore::revision::commit` in
   versioned mode].  These verbs open their own per-call write token against
   the workspace in the current working directory, which also keeps
   lore-drive's long-lived handles read-only.

7. **Paths passed to stage verbs** are workspace-relative (no leading `/`),
   with the process CWD being the workspace root — identical to CLI usage.

8. **Address materialization cache** (drive mode) is in-memory, keyed by node
   id and valid only for the currently served change-tag (cleared on every
   tree swap).  Cold-start listings of large staged trees will therefore
   re-`put` (rehash) files once per process lifetime + change-tag; that is
   accepted for v1 — `put_file` is idempotent and writes nothing when the
   content already exists.
