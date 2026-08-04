# Running the Memory-Based Revision Workflows over the gRPC Contracts

Implementation plan for the browser → Node server → Lore server flow: what
the wire surface and server backends already provide, what the new
`lore_revision_tree_*` capabilities need from them, and what still has to be
built to run the no-working-tree write workflow through gRPC without buffering
whole files in the Node layer. Companion to `PROGRESS.md` (the SDK-side implementation) and
`docs/proposals/2026-05-14-low-level-revision-api.md` (which explicitly
scoped gRPC out and anticipated this follow-on: *"The proposal does not
preclude exposing a subset of these operations as RPCs later for genuinely
remote callers; that is a separate LEP"*).

## Implementation progress

- [x] Prove nested empty directories survive commit and reload.
- [x] Add shared `lore_revision::revision_tree` edit primitives for add,
  exact directory creation, modify, delete, and move; keep the SDK verbs as
  handle/event adapters. The SDK revision-tree suite passes (104 tests).
- [x] Split immutable revision construction from branch publication.
- [x] Add bounded streaming content ingestion and `UploadContent`.
- [x] Add `RevisionCreate`, idempotency, limits, and shared authoritative
  publication orchestration.
- [x] Add server integration/end-to-end coverage and finish the contract docs.
- [x] Add the independent SDK remote-push follow-through (G1).
- [x] Add revision/path file downloads for thin clients: exact path
  resolution, pinned revision metadata, short-lived logical Lore URLs,
  bounded defragmentation streaming, propagated mid-stream errors, empty-file
  handling, and single-range HTTP resume support.

Implemented on 2026-07-14. The server test path now exercises
`UploadContent` → `RevisionCreate` → `ThinClientService.RevisionTree`, including
large multi-fragment input, top-level and nested empty directories,
idempotent retries, ordered modify/move/delete, changeset and upload limits,
tip conflicts, branch protection, and service-account bypass. Live
AWS/S3-backed persistence remains deployment-specific validation; the same
streaming writer targets the configured `ImmutableStore`, so the server sends
bounded fragments directly to the Lore S3 backend without a Node staging
file or bucket.

The revision/path download extension below was implemented on 2026-07-15.

The read-side browser flow is now implemented as
`ThinClientService.RevisionFileDownload`:

```text
browser ──repo/revision/path──> Node
Node ──RevisionFileDownload──> Lore gRPC
Node <──pinned revision + URL suffix── Lore
browser ──signed GET/Range──> Lore HTTP ──bounded fragment reads──> store
```

The RPC resolves `identifier` (including `number = 0` tip queries) or
`signature` to a concrete revision, follows the exact file path through
authorized repository links, verifies the file's full repository/context
association and logical size, and returns the resolved repository, address,
size, mode, filename, expiry, and `/v1/presigned/...` URL suffix. The Node
server can return that URL or issue a redirect after joining it to the public
Lore HTTP origin.

The signed URL streams the logical file through Lore rather than exposing raw
S3 fragment objects. It supports `Range: bytes=...`, `206`, `416`,
`Content-Range`, `Accept-Ranges`, `If-Range`, and an immutable address-based
`ETag`.
Fragment-list walks skip non-overlapping leaf payloads and slice boundary
leaves, so ranged downloads remain bounded. Stream items now carry errors;
missing or corrupt fragments terminate the HTTP body with an error instead of
being logged as a successful short response. Zero-address, zero-length files
return an empty body without a store read.

This feature requires the Lore HTTP endpoint and
`presigned_url_hmac_key`; when they are unavailable the gRPC operation returns
`FailedPrecondition`. There is deliberately no materialized S3 download cache
and no download file-size cap. Deployments should bound concurrent downloads,
bandwidth, and idle duration operationally rather than buffering or rejecting
large logical files.

---

## 1. The protocol landscape today

There are **two generations** of wire contracts, both served simultaneously
(`lore-server/src/grpc/server.rs` registers every service below):

### 1a. Legacy `urc.*` services

- Protos: `lore-server/src/legacy/proto/{storage,revision,repository,environment}.proto`
  (packages `urc.rpc` / `urc.model`; generated into
  `lore-server/src/legacy/generated/` and `lore-proto/src/grpc/urc.*`).
- This is what the **SDK/CLI talks to today**: `lore-transport` defines the
  `Storage` / `Revision` / `Repository` / … traits (`lore-transport/src/traits.rs`)
  with both gRPC (`lore-transport/src/grpc/`) and QUIC implementations. The
  storage handle's remote (`StoreInternal::remote` →
  `endpoint.session_connection(partition)`) hands out exactly these traits.
- Server handlers are shared with v1 where possible via
  `lore-server/src/grpc/handlers/*` (e.g. `branch_push.rs` backs both
  generations).

### 1b. `lore.*.v1` services (the go-forward contracts)

Protos in `lore-proto/proto/lore/`, implementations in
`lore-server/src/grpc/<service>/v1/`:

| Service | RPCs | Notes |
|---|---|---|
| `lore.storage.v1.StorageService` | `Get`, `GetMetadata`, `Put`, `Query`, `PresignDownload`, `Verify`, `Copy`, `MutableLoad`, `MutableStore`, `MutableCompareAndSwap` | Bidirectional streams for bulk transfer. `Put` **validates the payload hash server-side** (`protocol/storage/put.rs::validate_hash` via `lore_storage::hash_fragment`). Repository id travels as gRPC metadata (`REPOSITORY_ID_KEY`); identity via JWT interceptor. |
| `lore.revision.v1.RevisionService` | `BranchCreate/Delete/Get/List/Push`, `BranchMetadataGet/Set`, `RevisionList` | Charter (proto header): *"minimal, stable set of graph primitives."* `BranchPush` is the **only revision-graph write**: takes a revision signature already present in CAS, deserializes the state server-side, checks `parent_self` against the current tip (tip-collision → `FailedPrecondition` with the current latest embedded), verifies referenced fragments exist (`verify_fragments`), honors branch protection (service accounts bypass), runs pre/post hooks, emits notifications, and supports `force` / `fast_forward_merge`. |
| `lore.thin_client.v1.ThinClientService` | `RevisionInfo`, `RevisionTree`, `RevisionDiff`, `ContentDiff`, `RevisionFileDownload` | Charter: *"presentation helpers for clients that lack local cache or compute (web UIs)."* Read handlers build a pathless `RepositoryContext::new_server_context(...)` and walk `State` server-side — the very same machinery the revision_tree SDK surface uses. `RevisionTree` streams `TreeNode {path, node_type, address, last_changed_revision}`; `RevisionFileDownload` resolves an exact file and returns a pinned, short-lived logical Lore HTTP URL. |
| `lore.repository.v1.RepositoryService` | `RepositoryCreate/Delete/Get/List`, `RepositoryMetadataGet/Set` | Repo lifecycle incl. default-branch creation. Caller pre-generates ids (UUIDv7) for idempotent retries. |
| `lore.environment.v1`, lock, notification, admin | — | Not relevant to this workflow. |

### 1c. The new SDK surface (recap)

`lore_revision_tree_*` (this fork, see `PROGRESS.md`): load / resolve_path /
list_children / node_info / node_path / info / add / modify / delete / move /
metadata_set / metadata_get / commit / close, all against a storage handle,
no working tree. The commit pipeline (`lore_revision::commit::commit_tree`)
**already lives in `lore-revision`** — reachable from `lore-server`. The
per-verb edit logic (validation + node surgery + delta recording) currently
lives in the `lore` crate's verb bodies (`lore/src/revision_tree/*.rs`).

**Dependency fact that shapes everything below:** `lore-server` depends on
`lore-revision`, `lore-storage`, `lore-transport`, `lore-proto` — **not** on
the `lore` crate. Server handlers can call `commit_tree` today but cannot
call the `lore` crate's revision_tree verbs.

There is one additional boundary to fix before the server calls the commit
pipeline: `commit_tree` currently serializes the revision **and writes the
branch latest**. A server-side construct operation must not update the
authoritative branch before `BranchPush` protection, fragment verification,
hooks, notifications, and compare-and-swap run. The construction-only portion
must therefore be extracted from `commit_tree`; see **G4**.

---

## 2. What already works end-to-end (thick-client path)

A service that embeds the SDK (in-process `lore` crate) can run the full
no-clone write workflow against a remote server **today**, using only
existing wire contracts:

```
1. lore_storage_open            (remote endpoint configured)
2. lore_storage_put             → bytes land locally; content address returned
3. lore_revision_tree_load      (store, repository, base revision)
4. add / modify / delete / move / metadata_set  (in memory)
5. lore_revision_tree_commit    remote_write=1
     → tree blocks, name tables, delta block, metadata fragment, file
       fragments, and the 320-byte revision record are uploaded to the
       server CAS through the existing storage protocol (immutable::write
       with remote_write), and the *local* branch tip advances
6. urc.rpc / lore.revision.v1 BranchPush(branch_id, new_revision_signature)
     → server re-validates (tip CAS, fragment verification, protection,
       hooks) and advances the authoritative tip
```

Step 6 is the one seam that is not yet wrapped by the SDK surface: the
existing `lore branch push` verb is working-tree-based, and the transport's
`Revision::branch_push` is not yet called from any revision_tree verb. See
gap **G1**.

The server-side validation story required by the LEP ("commit through this
surface honors the same branch semantics") holds on this path because
`BranchPush` is the same gate the file-system flow goes through.

---

## 3. What a pure thin client (no SDK) can and cannot do

Can, today:

- **Read everything** the workflows need: `ThinClientService.RevisionTree`
  (tree by revision, path-prefix + depth filters), `RevisionInfo`,
  `RevisionDiff`, `ContentDiff`, `StorageService.Get/GetMetadata` (bytes by
  address), `RevisionService.RevisionList` / `BranchGet` (by name!) /
  `BranchList`.
- **Download one logical file by revision and path** through
  `ThinClientService.RevisionFileDownload`, then redeem the returned Lore HTTP
  URL directly from the browser with range/resume support.
- **Stream arbitrary raw file bytes into Lore** through `UploadContent`, then
  construct and publish an address-backed changeset through `RevisionCreate`.
- **Upload already-framed storage fragments** through `StorageService.Put`.
  This RPC validates the supplied fragment metadata and payload hash, but it
  is not a raw-file upload API: the caller must already understand Lore's
  chunking, compression flags, hashes, fragment-reference lists, and recursive
  fragment-list roots.
- **Advance a tip to an existing revision** through `BranchPush`.

Original gaps, now closed by this plan:

- `UploadContent` now streams an arbitrary file as raw bytes and returns a Lore
  address without requiring the caller to understand Lore fragmentation.
- `RevisionCreate` now turns a base revision plus path operations and uploaded
  content addresses into a constructed, authoritatively published revision.

The target thin-client flow is therefore two-phase:

```text
browser body ──stream──> Node ──UploadContent stream──> Lore ──> S3
                                      │
                                      └── Address + logical size

Node ──RevisionCreate(address-backed operations)──> Lore
                                      │
                                      └── revision signature + number
```

Node is a backpressure-preserving proxy. It must not write temporary files or
concatenate the request body into one buffer.

---

## 4. Gaps and required work

### G1 — SDK: remote tip advance from the revision_tree surface (small)

**Implemented.** `remote_write=1` now publishes through the remote revision
service after the local construction/upload. A lost push response is resolved
by querying the remote branch: observing the new revision is success;
observing a tip that moved from the expected base reports `BranchAdvanced`
and carries that tip on the terminal event. Other server rejections remain
commit failures. Remote publication failure invalidates the handle.

`lore_revision_tree_commit(remote_write=1)` uploads all revision data but
advances only the local tip. For an SDK pipeline that targets a server, add
the follow-through: after a successful commit with `remote_write`, call the
remote's `Revision::branch_push` and surface the server's accept/reject on the
commit terminal event. This is useful for thick clients but is not on the
critical path for the browser → Node → Lore flow.

### G2 — Storage: raw streaming content upload

**Implemented.** `lore_storage::write_content_stream` uses async FastCDC and
an incremental recursive fragment-list builder. The gRPC handler adapts the
HTTP/2 stream directly to `AsyncRead`, and
`feature.upload_content_max_bytes` optionally enforces actual bytes read
(default: no file-size cap).

Add a high-level client-streaming RPC to
`lore.storage.v1.StorageService`. Keep the existing `Put` unchanged as the
low-level fragment protocol.

```proto
rpc UploadContent(stream UploadContentRequest)
    returns (UploadContentResponse);

message UploadContentRequest {
  oneof part {
    UploadContentHeader header = 1; // exactly once, first
    bytes chunk = 2;
  }
}

message UploadContentHeader {
  // New file: UUIDv7 generated by the Node service. Modification: the
  // existing node's address.context. Zero may ask Lore to mint one.
  bytes file_id = 1;
  optional uint64 expected_size = 2;
  // Caller-generated UUIDv7 used to make an interrupted retry identifiable.
  bytes request_id = 3;
}

message UploadContentResponse {
  lore.model.v1.Address address = 1;
  uint64 size = 2; // actual logical byte count observed by Lore
}
```

Implement a streaming counterpart to `lore_storage::write_content`:

1. Adapt the tonic message stream to `AsyncRead`.
2. Feed it through `fastcdc::v2020::AsyncStreamCDC` using the same minimum,
   expected, and maximum sizes as the current buffer writer.
3. Hash/compress/store each bounded leaf through the existing
   `store_fragment` path so all current validation, deduplication, remote
   storage, and write tracking remain shared.
4. Build fragment-reference lists incrementally. Flush a full list page to
   storage and feed its reference into the next level instead of retaining one
   reference for every file fragment until EOF. This keeps memory bounded for
   very large files, including at the fragment-list levels.
5. Verify `expected_size` when supplied and return the root address plus the
   actual logical size.

With the AWS immutable store configured, this path naturally writes Lore
fragments to S3 and records Lore's DynamoDB metadata/associations. Do not add
browser-presigned writes directly into the final Lore bucket in v1: an
arbitrary S3 object is not a Lore fragment graph and bypasses hash validation
and repository/context association. If bypassing Node/Lore bandwidth later
becomes necessary, use a separate staging bucket plus a finalize/import RPC;
do not expose the final Lore object layout to browsers.

Keep bytes out of `RevisionCreate`, including for small files. A batched
small-content upload can be added later if profiling shows per-file RPC
overhead matters.

### G3 — RevisionService: one atomic RevisionCreate RPC

**Implemented.** The unary changeset handler validates and reserves the
request id, derives file sizes from immutable metadata, applies operations in
order, constructs without changing latest, and publishes through the shared
authoritative BranchPush orchestration.

Add one path-keyed changeset RPC rather than stateful 1:1 handle verbs. The
client has one logical operation; the server still separates immutable
revision construction from authoritative branch publication internally.

```proto
rpc RevisionCreate (RevisionCreateRequest) returns (RevisionCreateResponse);

message RevisionCreateRequest {
  bytes request_id = 1;              // caller-generated UUIDv7
  bytes branch_id = 2;
  bytes base_revision_signature = 3; // caller's observed tip; zero = initial
  string commit_message = 4;
  repeated MetadataEntry metadata = 5;
  repeated Operation operations = 6;
}

message MetadataEntry {
  string key = 1;
  bytes value = 2;
  uint32 format = 3; // validated against Lore's supported metadata formats
}

message Operation {
  oneof op {
    PutFile put_file = 1;
    CreateDirectory create_directory = 2;
    DeletePath delete_path = 3;
    MovePath move_path = 4;
  }
}

message PutFile {
  string path = 1;
  uint32 mode = 2;
  lore.model.v1.Address address = 3;
  // No caller-supplied size: derive it from stored fragment metadata.
}

message CreateDirectory {
  string path = 1;
  uint32 mode = 2;
}

message DeletePath { string path = 1; }
message MovePath { string source = 1; string destination = 2; }

message RevisionCreateResponse {
  bytes revision_signature = 1;
  uint64 revision_number = 2;
}
```

Server semantics:

1. Validate request byte/count limits, ids, paths, operation shapes, and
   idempotency key before loading a tree.
2. Confirm the authoritative branch tip equals
   `base_revision_signature`; return `FailedPrecondition` with the current tip
   on a mismatch.
3. Build a pathless `RepositoryContext`, deserialize the explicit base, and
   apply operations in request order through the shared edit primitives.
4. Batch-load `PutFile` address metadata, require every address to exist in
   the repository/context, derive each logical size server-side, and preserve
   an existing file's file id on modification.
5. Construct and serialize the new revision into immutable CAS **without
   changing the branch latest**.
6. Publish it through the same protection, hook, fragment-verification,
   notification, history-acceleration, and branch compare-and-swap path as
   `BranchPush`.
7. Return the published revision signature and number. A final CAS race is a
   `FailedPrecondition` carrying the newly observed tip.

Do not expose `force` or `fast_forward_merge` on `RevisionCreate` v1. A stale
web edit must fail explicitly so the Node application can reload and decide
how to reconcile it.

The branch update is the atomic visibility point. Immutable content and a
constructed revision may remain unreachable after cancellation or a CAS
loss; that is acceptable content-addressed-storage behavior and can be handled
by retention/garbage collection rather than rollback.

Use `request_id` for a real idempotency record keyed by repository + branch +
request id. A retry with the same request digest returns the original result;
reusing the id for different content is rejected. Deterministic addresses
alone are not enough because a lost successful response leaves the branch
already advanced past the request's base.

### G4 — Code movement and the construction/publication split

**Implemented.** Shared edits live in `lore-revision`; SDK verbs retain their
event/handle adapters. `construct_tree_revision` and the shared server
`publish_revision` boundary prevent pre-publication tip changes.

Because `lore-server` cannot depend on the `lore` crate, move the validated
tree-edit logic currently inline in
`lore/src/revision_tree/{add,modify,delete,move_node}.rs` into a
`lore_revision::revision_tree` module. Include the directory-create primitive
described in **G5**. Leave the `lore` verbs as event/handle wrappers around
those shared operations.

Also split the current commit pipeline:

- `construct_tree_revision(...)`: freeze edits, rehash directories, create the
  delta/history/metadata, and serialize the immutable revision; never load or
  update branch latest.
- `commit_tree(...)`: the SDK wrapper that performs its existing local
  tip-collision check, calls `construct_tree_revision`, and advances the local
  branch tip.
- A shared server publication orchestration used by both `BranchPush` and
  `RevisionCreate`: protection, pre-hook, fragment verification, CAS,
  notification, post-hook, and response hooks.

Do **not** implement the server path as current `commit_tree` followed by
`handlers::branch_push::push()`. Today `commit_tree` writes latest first;
`push()` can then take its "already current" early return before verifying new
fragments. `RevisionCreate` also must not call only the low-level `push()`
helper and accidentally skip the hooks/notifications owned by the handler.

### G5 — Explicit empty-directory support

**Implemented.** Exact directory creation is shared by SDK and server paths,
with commit/reload and thin-client tree coverage for top-level and nested
empty directories.

Empty directories are a v1 operation, not a deferred extension.

Current SDK status:

- `lore_revision_tree_add(kind = DIRECTORY)` already accepts an empty
  directory with `size = 0` and a zero address, calls `State::node_add`, and
  marks it `StagedAdd`.
- `commit_tree_freeze` records staged directories and `rehash_directory`
  computes the empty-directory hash/size and clears the staged bits.
- Read operations already represent an empty directory as a directory with no
  child events.

What is still required:

- Extract/add a server-callable `create_directory(state, parent, name, mode)`
  primitive in `lore-revision` and expose it as the path-keyed
  `CreateDirectory` operation.
- Define it as exact creation, not implicit `mkdir -p`: the parent must exist
  and be a directory; an occupied target path is rejected. Operations are
  ordered, so callers create missing ancestors explicitly first.
- Add a focused SDK commit/reload test proving a newly added empty directory
  survives revision serialization. The existing add tests prove the
  in-memory mutation but do not currently cover commit + reload for an empty
  directory.
- Add server tests for a root empty directory, nested empty directory,
  directory beside files, missing parent, occupied path, move, and delete.
  The end-to-end test must confirm `ThinClientService.RevisionTree` emits the
  empty directory after `RevisionCreate`.

### G6 — Service placement

`RevisionCreate` belongs in `lore.revision.v1.RevisionService`: it is an
authoritative revision-graph write and shares the `BranchPush` publication
gate. Keep `ThinClientService` read-only. `UploadContent` belongs in
`lore.storage.v1.StorageService` because it turns bytes into a content address
without revision semantics. No new one-RPC services are needed.

### G7 — Repository bootstrap for the no-clone flow

Creating a repository and default branch remotely already works through
`RepositoryService.RepositoryCreate`, and `BranchGet(name)` resolves branch
names to ids. No new RPC is required; the Node layer should resolve once and
use ids for upload/create calls.

### G8 — Tests and contract documentation

**Implemented for the in-memory/local-store server suite.** Live S3-backed
verification is intentionally left to an environment with the configured AWS
backend and credentials.

- Storage unit/integration tests: empty content, below/at/above fragment
  threshold, many chunks, recursive fragment-list levels, cancellation,
  declared-size mismatch, retry with the same file id/request id, bounded
  memory/backpressure, and AWS-backed persistence where available.
- Revision handler tests mirroring `branch_push.rs`: tip collision, protected
  branch, service-account bypass, hooks, notifications, idempotent retry,
  deleted branch, and a CAS race after construction.
- Construct-specific tests: every operation kind, duplicate/conflicting paths,
  unknown base, address absent from CAS, context/file-id mismatch, server-side
  size derivation, empty operation list, and the empty-directory cases in G5.
- End-to-end: `UploadContent` → `RevisionCreate` →
  `ThinClientService.RevisionTree` / `RevisionService.RevisionList`, including
  at least one large streamed file and one empty directory.
- Proto documentation: first-message upload header rules, cancellation and
  orphan behavior, size/count limits, idempotency, conflict details, and the
  exact branch-publication semantics.

---

## 5. Settled design decisions

- **Q1 — File ids.** The Node service generates UUIDv7 for new files and
  reuses the existing `address.context` for modifications. The upload header
  carries that id; Lore may mint one only when omitted. The browser does not
  implement Lore identity rules.
- **Q2 — Upload framing.** Add `StorageService.UploadContent`; browser/Node
  callers stream raw bytes and Lore owns fragmentation. Keep bytes, including
  tiny inline payloads, out of `RevisionCreate` v1.
- **Q3 — Tip advance.** `RevisionCreate` is one construct-and-publish RPC to
  the caller, but internally immutable construction and authoritative branch
  publication are separate. The final shared BranchPush CAS is the visibility
  point.
- **Q4 — Operations.** V1 includes `PutFile`, `CreateDirectory`, `DeletePath`,
  and `MovePath`. Empty-directory creation is required now; links remain
  additive follow-up work.
- **Q5 — Limits.** `RevisionCreate` limits are changeset limits: encoded
  request bytes, operation count, metadata count/bytes, and path sizes. Keep it
  unary; streaming operations would bypass the protobuf envelope limit but
  would not bound the mutated `State` retained until publication. Configure a
  cap and return `ResourceExhausted` with the applicable limit.
- **Upload size limits.** File size is independent of `RevisionCreate`. A
  correct streaming writer does not need a small file-size cap for memory
  safety, but a browser-facing deployment still needs configurable per-upload,
  per-user/repository quota, concurrency, bandwidth, deadline, and idle limits.
  Enforce total bytes incrementally; do not trust only the declared size or
  HTTP `Content-Length`.
- **S3.** S3 is the durable backend behind Lore, not a temporary Node handoff
  and not a final bucket browsers write into directly. A staging-bucket import
  flow is a later bandwidth optimization if measurements justify it.

---

## 6. Suggested order of work

1. **G4 + G5** — extract the shared edit primitives, add exact empty-directory
   creation, add commit/reload coverage, and split immutable construction from
   branch publication while keeping the SDK tests green.
2. **G2 storage core** — implement the bounded streaming content writer and
   incremental recursive fragment-list builder in `lore-storage`.
3. **G2 + G3 protos/services** — add `StorageService.UploadContent` and
   `RevisionService.RevisionCreate`, generated bindings, registration, and
   configuration limits.
4. **G3 server handler** — apply address-backed operations, construct without
   advancing latest, and publish through the shared BranchPush orchestration.
5. **G8** — complete handler, cancellation, conflict, empty-directory, large
   upload, and end-to-end tests plus proto/contract documentation.
6. **G1** — add SDK remote-push follow-through independently or in parallel if
   the thick-client path is also needed.
