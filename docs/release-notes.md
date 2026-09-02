# Release Notes

Release notes for the open source Lore project. Releases before v0.8.4 predate this file; see the
[GitHub releases](https://github.com/EpicGames/lore/releases) page for the published record.

## Nightly

### Breaking changes

- C API: `LoreGlobalArgs.no_atime` is removed. Nothing set it and nothing read it — the accessor it gated was never wired to the store settings, so the flag named a behavior that did not exist. Rebuild against the new `lore.h`; a caller that zero-initializes the struct needs no other change
- `lore-server`: `connection_message_limit` under `[server.quic]` / `[server.quic_internal]` is renamed `stream_message_limit` and applies per stream rather than per connection, so with `max_bidi_streams = 8` a value of 500 allows 4000 requests in flight per connection instead of 500

### Features

- `lore commit --stats` / `lore push --stats` report what the operation cost: files by the action each was staged with, and fragments by what became of them — already stored, compressed, written to the local store, duplicated as an association by the peer or uploaded. Reported once when the operation finishes, on the path that fails as on the one that succeeds, through the `RevisionCommitStats` and `BranchPushStats` events; `--stats=2` adds the per-fragment `FragmentWrite` stream. `--event-interval <milliseconds>` paces the progress events
- `lore-storage`: the local store records when an entry was last read, so eviction and compaction rank by what commands actually read rather than by when content was written. An entry is serialized inside its whole bucket, so the read stamp marks a bucket for rewrite only once the time it holds moves by an hour or more: a burst of reads costs at most one index write per bucket instead of one per read, a smaller move rides to disk with whatever writes the bucket next, and marking never schedules a flush of its own
- `lore-server`: request admission is per QUIC stream rather than per connection, so a saturated stream no longer stalls the others, and a connection-wide ceiling sheds immediately once requests in handling — including those waiting for a stream permit — reach `connection_inflight_limit`. The wait for a permit is bounded by `permit_timeout_ms` (default 100ms) rather than by the request deadline, and the two refusal paths report separate `AdmissionLimit` and `PermitTimeout` metric labels
- `lore`: `lore_storage_get_file_resolved` / `lore_storage_put_file_resolved` are the file-backed pair of `lore_storage_get_resolved` / `lore_storage_put_resolved` — a foreign key resolved straight into a file, and a file published under one, so an application never loads the content to write it and never buffers it to read it back. Lore streams both directions a fragment at a time, writing a fragment tree into the target at each leaf's own offset, so peak memory follows the fragment rather than the content. Publishing still costs one round trip whatever the size: content that fits a single fragment carries its key on that fragment's upload, and content that fragments carries it on the fragment list root's — the last fragment written, by which time every leaf's placement is known, so a tree that did not reach the remote whole still leaves its key unpublished. `lore_storage_put_resolved` gets that second saving too. `offset`/`length` select a range as they do for `lore_storage_get_file`
- A child directory holding its own `.lore/` is a nested working copy and bounds every walk of the parent's working tree: `status`, `diff` and `stage` stop at it rather than indexing its contents. Naming one on `lore stage`, or a file inside one, is refused. A directory the current revision holds stays tracked, since untracking committed content is an explicit user action

### Fixes & Improvements

- `lore`: `lore_storage_get_file` no longer replaces the destination when `offset` starts past the end of the content. The range resolved to nothing, but the file was still created, truncated and renamed into place before the call reported `INVALID_ARGUMENTS`, so a rejected request destroyed a destination that was already there. The target is now left alone, which is what the C API documented; a start exactly at the end is still a legitimate empty read and writes the empty file
- `lore`: `lore_storage_put_file` and `lore_storage_put_file_resolved` reject a missing or non-regular `path` as `INVALID_ARGUMENTS` on the first open, rather than after the ten-second transient-failure back-off. A directory can no longer retract a published key by reporting zero size. The check moved onto the handle's own stat, costing one fewer `statx` per item
- `lore-storage`: a write that waits on another task already storing the same address reports the placement the store settled at rather than the one it read before waiting, so content whose fragment tree repeats a leaf — a file of identical blocks — is no longer reported as partly absent from the remote, and a key naming it is published instead of withheld
- `lore-storage`: a stopped garbage collection pass gives up within one packfile instead of after a whole compaction step, so process exit waits at most one packfile rewrite; the stop at the end of every repository command is gone, so background eviction and compaction continue across commands
- Fix a directory staged as an add and then removed before any commit being reported as a delete no command could clear; a scan now discards the entry with its whole subtree, as it already did for a reverted single-file add
- Fix `stage --scan` keeping an entry `status --scan` discards, which left the two walks with different trees
- `lore branch merge`, `branch merge into` and `revision cherry-pick` carry nothing from the source revision's metadata unless `--inherit-metadata <KEY>` (repeatable) names it, so `merged-by`, `reviewed-by` and `change-request` record whoever performed the merge rather than whoever produced the branch; actor and provenance keys stay reserved even under `*`
- Fix `lore branch merge` refusing files that carry no local changes: the working file is measured against the node the current revision holds rather than against the base of the three-way diff, a comparison is answered by the chunking the content was stored under, and one file is read and hashed at most once however many revisions it is measured against

## v0.9.0 (Aug 28th 2026) [#782]

### Breaking changes

- `lore-credential`: auth tokens move to `tokenstore.toml` under a `tokenstore_encryption_key` key, with nothing migrated from `tokens.toml`; old and new clients keep entirely separate credentials, so expect one `lore login` per generation. An unmigrated store reads as `Not authenticated` and `--remote` reads answer empty rather than failing, so scripts gating on `lore status` should check the exit code
- `lore-server` (AWS store): the fragment describing a payload now travels on the S3 object as an `x-amz-meta-lore-fragment` header instead of in a DynamoDB record, and the fragment metadata table is replaced by a fragment state table holding lifecycle state alone (a row's presence means the hash exists). Existing objects are not rewritten — they are read through a fallback to the old table, which must stay configured for them to remain readable. Under `[plugins.aws.immutable_store]`:
  - Existing deployment: set `dynamodb_fragment_state_table` (required, no alias — start fails without it; normally the table that held fragment metadata, since the key schema is unchanged and the two row shapes coexist) and `dynamodb_fragment_metadata_table` (enables the fallback read, normally the same value; accepts the older `dynamodb_metadata_table` spelling). Roll out as a full stop followed by a full start — old and new servers must never run at the same time. To move the data across and retire the old table, see `contrib/aws-migrate-0.9.0/README.md`
  - New deployment: set `dynamodb_fragment_state_table` only. Leaving `dynamodb_fragment_metadata_table` unset declares that no object predating the change exists, so no fallback read is ever issued and an object without its own metadata is reported as damaged
- `lore.model.v1` and `lore.thin_client.v1`: `Repository.created`, `Branch.created` and `Revision.timestamp` now carry Unix epoch milliseconds instead of seconds
- C API: `LoreMetadataType` discriminants are now stable integers shared between `lore.h` and the on-disk metadata buffer, and `lore_revision_tree_metadata_set` takes typed `(key, LoreMetadata)` batches instead of text plus a format tag; callers using the enum names need a recompile, and callers hard-coding the old numeric values must update them
- `lore-server` (replication protocol): `ExistsBatch` is replaced by a batch `Query`, and `Get` / `GetMetadata` responses carry the full `StoreGetData` including the partition. Retired opcodes are reserved rather than reassigned, so a peer on the previous protocol is rejected instead of misreading a response — roll replicas and their upstream together
- C API: failures that previously reported `-1` (internal) now carry the specific code where one applies, because an error crossing an internal boundary keeps its variant instead of collapsing. `lore_revision_tree_metadata_set` and its file equivalent can now return `SlowDown` (5), `NotAuthorized` (7), `Maintenance` (11), `NotAuthenticated` (12), `NoRemote` (14), `NotConnected` (17) and `NotSupported` (18); remote store reads and writes add `Disconnected` (6); and connecting with no stored credential reports `NotAuthenticated` rather than an internal fault. A caller treating every non-zero status alike is unaffected; one that branches on `-1`, or reads `-1` as `retrying will not help`, now sees retryable and re-authenticable codes on paths that previously only ever produced `-1`, and should handle them before upgrading

### Features

- `lore-io`: new runtime-independent asynchronous file I/O engine backing Lore's file access — positional owned-buffer operations on a bounded, idle-reaped syscall pool, upgraded automatically to `io_uring` on Linux and overlapped I/O on Windows, with vectored scatter/gather reads and writes. `LORE_IO_BACKEND` overrides the choice; internals in `docs/developing/internals/file-io-engine.md`
- `lore`: `lore_revision_tree_commit` freezes a handle's in-memory tree into a revision and advances the branch tip by compare-and-swap, taking the branch from the handle's `branch` metadata key. Commit is exclusive and all-or-nothing — a failure leaves the handle where it was with every edit still staged — so services can publish revisions with no working tree on disk
- `lore`: `lore_revision_tree_delete`, `_modify` and `_move` join `add` as batch verbs, each validating the whole batch before any node changes and emitting a `*_COMPLETE` per entry plus one `BATCH_COMPLETE`. A node from the loaded revision is staged for deletion and reversible, one added through the handle is discarded outright, and `staged_action` on the child and node-info events reports what is pending. Moving into a linked repository is not supported yet
- `lore`: batched `lore_revision_tree_metadata_set` / `_get` / `_clear`, with `LoreMetadata`, `LoreMetadataType` and `LoreBinary` reworked into owning, self-describing types so binary metadata can cross event callbacks and both wire formats (see Breaking changes)
- `lore`: a revision tree handle holds its own store reference, so closing the parent storage handle leaves it usable; orphaned handles are closed when an IPC connection drops, and `lore::shutdown` drains tree handles first
- `lore`: `lore_storage_get` / `lore_storage_get_file` take `offset` and `length` per item to read only a slice of the content, pruning the fragment tree so the work is proportional to the range rather than the content size (a zeroed pair still reads the whole content; a start past the end is rejected with `INVALID_ARGUMENTS`)
- `lore`: `lore_storage_get_resolved` / `lore_storage_put_resolved`, also over QUIC and gRPC, resolve a key belonging to another system — an asset id, a build id — and act on the content it names in one request instead of two. A read resolves local-first and caches the mapping it learns, verifying the root fragment against the resolved hash; a write stores content before publishing the key and is last-writer-wins. Design: `docs/proposals/2026-08-02-resolved-storage-operations.md`
- `lore branch archive --include-layers` / `--layer <path>` archives the branch in every configured layer, or in the layer at one mount path; the default still touches only the repository it ran in, since archiving deletes and a layer owns its own branch lifecycle
- `lore --identity-token <token>` / `--access-token <token>` (and the matching `LoreGlobalArgs` fields) use caller-supplied tokens instead of the credential store, for CI runs and stateless services. Tokens are ephemeral and never stored, the identity is read from the token, and both conflict with `--identity`; given only an access token, an operation needing an identity token fails rather than falling back to a stored one
- `LoreRepositoryCreateArgs` / `LoreRepositoryCloneArgs`: `use_shared_store` is replaced by a `LoreSharedStoreMode` enum (`Inherit` = 0, `Enabled`, `Disabled`), so a caller can explicitly refuse shared-store backing on a machine where `use_shared_store_automatically` is set (a zero-initialized struct still follows the machine setting)
- `ImmutableStore`: `exist` / `exist_batch` are unified into a batch `query` answering with the best match level found, and `get` / `get_metadata` drop their match-level parameter for one `StoreGetData` carrying the fragment, the level and an optional payload. The contract — never over-report, obliterated never matches, reads agree with each other and name where a match was found — is written into the trait and enforced by a conformance battery every store runs
- `lore-server`: fragments arriving Oodle-compressed on `put` are transcoded to Zstd as the first step of retiring Oodle, gated behind the `oodle` feature and opt-out at runtime with `LORE_DISABLE_CONVERT_OODLE_ON_PUT`; the address is unchanged, since identity is over uncompressed content
- `lore-aws`: `MetadataMigrator` and `run_migrator` drain legacy DynamoDB fragment-metadata rows into the new S3-object-metadata plus state-row model, by segmented parallel scan across a pool of consumers. An accurate non-Oodle codec is re-uploaded as-is to set its S3 headers, Oodle and mismatched codecs recompress to Zstd, and a run is idempotent and resumable. Packaged as a standalone tool under `contrib/aws-migrate-0.9.0`
- `lore link add` / `remove` / `update` / `reset` / `list` now work on a nested link — one mounted inside another link's subtree — mutating the innermost repository's registry and propagating outward, and `lore commit --link <nested path>` commits each intermediate link as a real revision before repinning. Nested-link merge is not covered yet
- `lore-server`: the legacy `urc.rpc.RevisionService` diff and tree gain `link_partition` and `tracking` fields, so a consumer can tell which repository a changed path resolves under and whether a link follows its parent branch or is pinned (both additive on messages already marked deprecated)
- `lore-server`: `presigned_url_extra_content_types` and `presigned_url_denied_content_types` under `[server.http]` adjust which `Content-Type` values a redeemed presigned URL may carry. They extend rather than replace the built-in safe set, a type in both lists is denied, and browser-executable types can never be added — the server refuses to start instead of failing open
- `lore-proto`: thin-client `TreePath` exposes file size and mode, so a `ThinClientService.RevisionTree` caller gets both without a second request
- `lore`: self-signed certificates installed in the OS trust store are now honored (`reqwest`'s `rustls-tls-native-roots`), which local development against a self-signed server needs
- `lore link info <path>` reports one link's mount and source paths, its branch and whether that branch is followed or pinned, the pinned and remote-latest revisions, the link flags, and the staged state and staged file count inside it; the remote revision is omitted when offline

### Fixes & Improvements

- Thread model: the network transport moves onto its own runtime, the rayon compute pool is gone with compression, hashing and chunking now inline on the core workers, and the blocking pool shrinks to two threads. `LORE_MAX_THREAD` is now an absolute cap. Clone throughput 1.4 → 2.5 Gbps, large-commit peak memory 6 → 4 GiB
- `lore-storage`, `lore-revision`: the store buckets, packstore, fragment chunker and remaining `tokio::fs` / `std::fs` calls all run through `lore-io`, so a common-case bucket load completes in one dispatch and a flush gathers header, index and entries in one vectored write; filesystem locks back off asynchronously instead of parking a thread
- `lore-storage`: fix a file shorter than its measured size being taken as an early end, producing a chunk list covering fewer bytes than its root fragment recorded; the read now fails
- Windows: no handle the I/O driver opens permits a second writer, while a read-only open still shares reads and deletion so replace-by-rename works. `ERROR_SHARING_VIOLATION` is now transient and retried, so materializing a file being hashed waits rather than fails
- Fix `lore branch merge` under a sparse view dropping changes outside the view, leaving the branch divergent from the one it recorded as merged; nodes now merge regardless of the view, which gates only on-disk work, and an out-of-view conflict adopts theirs as `StagedMergeTheirs` (adopting untouched excluded subtrees whole made it 34% faster on 110,000 files)
- Fix a link added, removed or repinned on one branch not surviving the merge that brought it over, where `link list` reported nothing and `lore stage` failed with `Link not found`; the registry now follows any link node a change set stages or deletes, and a row both sides moved refuses the merge
- Fix a revision moving a link's pin producing an empty diff when the subtree was byte-identical, and entries from a linked repository naming no repository so content could not be fetched from the right partition; a new `link_read` policy gives a caller authorized only for the parent one change for the mount path
- Fix `lore branch create` failing with `Branch <name> already exists` when a linked repository held the requested branch id under another name; the cascade adopts it and reports a `LinkBranchCreate` event with a `reused` flag
- Fix concurrent link edits overwriting one another: `link add`, `update` and `remove` released the runtime lock between reading and storing the registry, and now hold one write lock across it
- Fix `lore push` on a repository with links reporting the parent's revision against a branch that exists only in a linked repository; push events now carry the repository and branch they report (repository state was always correct)
- Fix `lore stage . --scan` pinning a staged revision for unchanged layers, which aborted the next `commit` with `Nothing staged` and made `branch switch` refuse with `Layer has uncommitted staged changes`; a layer is pinned only when the walk left its state dirty
- Fix `lore sync` leaving a layer's staged state on the revision it moved away from, so the next commit refused it as a stale parent or under `--force` reverted what the sync brought in; sync now refuses when a layer holds staged work and otherwise rebases onto the new pin
- Fix `lore layer remove` discarding a layer's staged work silently and leaving staged adds on disk; removal now counts staged files and refuses without `--force`
- Fix an unreadable `.lore/config.toml` or `layer.toml` presenting as a repository with no remote or no layers, which the next save made permanent; both now default only for `NotFound` and save through a renamed `.tmp` sibling
- Content the peer already holds is no longer re-uploaded: where a query reports the hash under another context or partition, the client asks the peer to duplicate the association instead of sending the payload, on `lore push` and direct writes alike
- `lore-revision`: the composite store caches metadata resolved by `query`, not only `get_metadata` hits, so a `query`-heavy workload stops hopping to the durable store; capped by `cache_metadata_semaphore_size` and limited to durable-sourced results, and `should_cache_query_results` is renamed `cache_metadata`
- `lore-server`: `get_metadata` now fans the durable store out in parallel with read replicas and sends a dedicated QUIC command, instead of falling back to a local `query` that reports only the durably-stored flag; edge replicas answered incorrectly and always paid a cross-region trip
- `lore-server`: the JWK service picks up key rotation without a restart; a verification failure only a key could explain triggers one refresh, so material replaced behind an unchanged `kid` no longer rejects every token
- `lore-server`: JWK fetches are bounded by timeouts, a pooled client, single-flight collapsing of concurrent misses and a minimum interval, with an empty cache never throttled so start-up always proceeds. The document is capped at 1 MiB
- `lore-server`: a JWK set is validated key by key — an unusable key is skipped rather than failing the fetch, a key whose `alg` family does not match its `kty` is refused, and an empty result errors. RS256 is inferred for an RSA key omitting `alg`, which previously failed start-up against Microsoft Entra ID
- `lore-server`: why a JWT verification failed no longer reaches the caller — `bad signature`, `expired` and `no such key id` are an oracle for someone unauthenticated; the reason goes to a debug log
- `lore-server`: fix a stored-XSS vector on presigned-URL redeems, where bytes could be served from the Lore origin under a caller-chosen `Content-Type` such as `text/html`; a deny-by-default allowlist rejects at mint, coerces on redeem, and every response carries `nosniff` and a `default-src 'none'; sandbox` CSP
- `lore-server`: `binary/octet-stream` is allowed in the presign allowlist and served verbatim — S3 assigns it to objects uploaded without an explicit `Content-Type`, so callers forwarding S3 metadata had theirs coerced
- `lore-server`: `RepositoryMetadataGet` and `RepositoryMetadataSet` now perform the real-time authorization check the other repository handlers do, so a client cannot read or write metadata on a repository it has no access to
- `lore-transport`: fix a per-item failure on the streaming storage RPCs being reported as a stream trailer, which killed the HTTP/2 stream and every request multiplexed onto it; outcomes now travel in-band, and field numbers are preserved so get responses stay wire-compatible
- `lore-transport`: fix gRPC reconnect not re-reading the epoch per attempt, and storage streaming having no working reconnect at all; a dead stream now rotates once across concurrent callers and replays outstanding requests
- Fix `lore` commands taking about 30 seconds when a hostname resolved first to an address family the server was not listening on, commonly `localhost` on `::1`; the QUIC client now follows Happy Eyeballs (RFC 8305), 30.05 s to 0.38 s on the reproduction
- `lore-transport`: fix the per-stream in-flight counter never being decremented on error or cancellation, which degraded priority routing and left the selector round-robining; also fix `add_stream` storing a count one short of the streams it opened
- `lore-aws`: `query` is backward compatible with fragments described by the legacy metadata table, so `BranchPush` no longer reports content as missing when it is stored but described by the old row
- `lore-aws`: the mutable store's zero-expected compare-and-swap matches the local store now — a row holding a zero value is treated as absent — so an empty branch pointer cannot make a swap report success without the write landing
- `lore-storage`: a corrupt mutable-store bucket is no longer reset to empty on an authoritative store, where it silently dropped branch heads, metadata and instance registrations with no upstream to refill from; `authoritative: true` makes it a hard error
- `lore-credential`: fix the credential store rewriting its keyring entry on every stored token, which on macOS prompted for keychain access from every application linking Lore and again after each rebuild; each seal now draws a random nonce instead of persisting a counter
- `lore-credential`: fix any keyring error being read as `no key`, so one denied prompt regenerated the key and wiped the token store for every Lore process on the machine; only `NoEntry` counts as absent
- Fix a notification subscription that had stopped on its own still counting as live, so every later subscribe returned success without subscribing; liveness is now checked by cancellation token and task state
- Fix `lore history --branch <name>` returning an empty listing for a branch that exists only on the remote; the local branch is preferred and the remote head used when there is no local history
- Fix an infinite loop walking revision history for a deleted file, where a zero parent hash was handed to `State::deserialize` repeatedly; a three-way diff base no longer resolves to the zero revision
- `lore-revision`: the diff and merge base search no longer errors as `Divergent` when it cannot prove divergence — branch points sharing no revision fall back to the older one, and two points with the same revision number short-circuit instead of spending a full search budget
- Fix `lore reset <path> --revision <revision>` failing when the path is currently a directory but was a file in that revision
- Fix `lore sync` undoing a change from a file to a directory; where an add and a delete carry the same node name, the delete now takes the `from` node
- Fix a clone failing with `Failed to create directory` when the parent already existed but was not created by that call — a bind-mounted clone root or a drive root
- Fix `lore stage --targets <file>` hanging on a large list, where `dedup_to_supersets` rescanned every kept path per candidate and timed out around 900,000 paths; sorting in subtree order takes the collapse from quadratic to O(n log n)
- `lore stage` no longer grows memory in proportion to the work ahead of it; in-flight tasks are capped at 1000 and drained as they complete, and directory fan-out is bounded by a semaphore, processing a child inline when no permit is free
- `lore-revision`: node allocation during staging no longer holds a single-permit mutex across its whole scan, and `node_add` skips the 65 KB zero allocation it issued when clearing a recycled slot whose metadata block was never written
- `lore-revision`: the per-node stage log lines drop from debug to trace; they fired once per file and made `--debug` unusable during a large stage
- `lore-base`: fix `Hash`, `Context`, `Partition` and `Address` being unreadable under non-self-describing formats such as `bitcode`, which made any record carrying one fail to decode; a truncated address is also refused rather than becoming the zero address
- `lore-server`: pushing a revision hash that does not exist in the immutable store returns `NotFound` instead of a generic `Internal` status
- `lore-server`: `LORE_SERVER_MAINTENANCE=1` stops the internal QUIC server serving its port while the internal gRPC server keeps a stub listening, so health checks get `unavailable` rather than a closed port
- `lore-telemetry`: `size_histogram` gains explicit power-of-two boundaries from 64 B to the 256 KiB `FRAGMENT_SIZE_THRESHOLD`, so `put` and `get` distributions are no longer cut off at the OpenTelemetry defaults
- `lore-error-set`: new `chain_err_from` chains a discrete error onto an error-set enum without destructuring a `Traced<E>`; call sites that discarded the originating trace now preserve it
- `lore-revision`: `LoreRevisionDiffFileEventData` gains `from_path`, so a receiver of `LORE_EVENT_REVISION_DIFF_FILE` can reconstruct where a moved or copied file came from
- `lore`: the batch verbs' id fields are named apart — `entry_id` per entry, `batch_id` on the batch args and `BatchComplete` — and `parent_entry` becomes `parent_entry_index`
- SWFS groundwork: the stale commented-out integration is replaced by a real interface whose types are always available while its methods compile only under the `swfs` feature; `InstanceOperationImpl` lets an operation in a linked or layered repository be finalized alongside the main one
- New `iteration` Cargo profile inherits `release` with LTO off, cutting link time when rebuilding frequently
- `lore-revision`: reject directory traversal and dot-prefixed segments in repository path components
- `lore-aws`: guard against maliciously large S3 payloads
- `lore-storage`: fix data loss in the lazy fan-out redistribute path, where buckets left unmarked after a fan-out could be overwritten from the stale on-disk layout by a reader racing the flush
- `lore-storage`: decide the legacy bucket layout per group rather than per store, so one written group no longer pins every other group at the maximum bucket count on the next open
- `lore-storage`: file modification detection no longer downloads content — `file_matches` transfers the stored header and, when fragmented, its fragment lists, then compares chunks against the file's own bytes
- `lore-storage`: fix a dropped consumer on a streaming read being reported as an error instead of draining gracefully
- `lore-storage`: build zstd compression and decompression contexts in workspaces this crate owns, with the pool holding at most 32 and further concurrency building one per call
- `lore-storage`: remove `allow_partial_fragment` from the local store, so a `lore-server` local store can cache `get_metadata` results
- `lore-storage`: obliterate a fragment tree without holding a lock across it, and drop an unreachable sink path from the defragment pipeline dispatch
- `lore-storage`: key file modification times by the lowercase path string, unifying them with node-name matching
- `lore-server`: `ReplicatedImmutableStore` implements the `Copy` message and returns the `Context` a query matched under, instead of leaving clients to assume the default
- `lore-transport`: a persistent connection updates the authn and authz tokens it presents, and authz exchange results for caller-supplied identity tokens are no longer written to the token store
- `lore-auth`: a missing token reports `NotAuthenticated` instead of a generic internal error
- Fix clone not applying view filtering inside linked and layered repositories, where `clone_node` was called with a path relative to the linked repository rather than the mount point
- `lore-revision`: refuse a `link remove` that would destroy local edits
- `lore-revision`: fix non-ASCII lowercase handling when building relative paths
- Fix the originating trace being lost through `::internal` and `::internal_with_context`
- `lore-revision`: staging does less work per path — targets resolve concurrently, each walks from its pre-created ancestor rather than the root, a directory's children are read once and matched by binary search, a shared directory's case resolves once, and paths are neither re-stat'd nor rebuilt through extra string allocations
- `lore-revision`, `lore-storage`, `lore-base`: allocation and locking trimmed on the hot paths — the dirty tree walks from an explicit stack and names into one buffer, node blocks serialize from the lock without a copy and deserialize once rather than once per waiter, merkle tree blocks come from their own heap, short ASCII names fold to lowercase on the stack, the log level reads from an atomic, and successful operations no longer allocate error strings
- `lore-transport`, `lore-revision`: reuse the lazy `StorageSession` wrapper across fragment writes instead of rebuilding it per write
- `lore-io`, `lore-revision`: verify a path's case by asking the filesystem for the name instead of listing its directory
- `lore-aws`: box SDK error payloads to shrink the `lore-aws` error types

## v0.8.6 (Jul 29th 2026)

### Breaking changes

- `lore-client`: logging CLI arguments are now strictly mutually exclusive; a command line passing conflicting logging flags is rejected instead of silently accepted
- Auth `login`/`info` now return `NotSupported` (code 18) when the server has no auth endpoint configured, and surface `NotAuthenticated` / `Disconnected` instead of a generic internal error (code -1); scripts branching on these exit/FFI codes must be updated
- Presigned URL vending (`POST /v1/repository/{id}/content/{address}/presign`) is now restricted to service accounts; normal user tokens can no longer mint presigned URLs

### Features

- `lore-server`: forward `BranchList` and `RepositoryCreate` to a remote Lore server when configured (extending the existing `BranchCreate`/`BranchDelete`/`BranchGet` forwarding)
- `lore`: add a batch node-add verb (`lore_revision_tree_add`) to the low-level revision API, landing a whole subtree atomically
- `lore`: run the service process (`lore service run`) on Linux and macOS, not just Windows
- `lore`: validate that all strings passed to the C API are valid UTF-8, rejecting invalid input up front with a named field
- `lore-server`: support file:// JWKS endpoints in the JWK service
- `lore-revision`: optional `durable_delay` on the composite store so read replicas can answer before the durable tier is queried
- `lore-storage`: self-heal corrupt (torn-write / zero-filled) mutable store buckets instead of failing to open the store
- `lore-server`: add `Cache-Control` headers to the presigned-URL redeem endpoint for immutable content
- Add a self-contained Terraform example for an AWS primary + edge deployment under `contrib/aws/`

### Fixes & Improvements

- `lore-storage`: `read_into` now respects the requested byte range for single-fragment reads (partial reads of files ≤ 256 KiB no longer fail)
- `lore-server`: fix JWK refresh cache check so a missing/rotated key triggers a fetch, allowing key rotation without a restart
- `lore-credential`: require a DNS label boundary when matching a dotless JWT `aud`, and allow exact apex-domain matches, closing an audience-suffix leak
- `lore-revision`: preserve dirty move status through `status --scan` instead of degrading it to delete + add
- `lore-revision`: bound commit read memory with a travelling fragment permit, and cap directory recursion fan-out during commit
- `lore-revision`: drain staging tasks on all error paths so failures propagate cleanly
- `lore-storage`: batch FastCDC chunking to cut per-chunk overhead on medium/large writes
- Add transport connect timeouts so local reads no longer stall on an unreachable remote
- `lore`: use the calling process's working directory for absolute-path resolution in the service process
- Fix `lore_branch_switch` and `lore_branch_reset` to restore the branch name→id mapping (so the branch appears in the local list), with a `--force` override
- `lore-revision`: fix `restore` failing with `Dirty node remain after nodes were committed`
- `lore-base`: update vendored `rpmalloc` to 2.0.1

## v0.8.5 (Jul 15th 2026)

### Features

- Implement the low-level revision-tree read verbs on the C API: `tree_load`/`tree_close`, `resolve_path`, `list_children`, revision & node `info`, and `node_path`
- Forward `BranchCreate`, `BranchDelete`, and `BranchGet` to a remote Lore server, opt-in per-RPC under `[server.grpc_public_services.forwarded_requests]`
- Add `repository info --local` to read repository metadata from the local store without contacting the remote

### Fixes & Improvements

- Fix a corrupt-tree race in concurrent `node_add` where a half-initialized node could be observed on the child chain
- Protect non-durable local-only fragments from being orphaned by store GC eviction/compaction
- Fix dirty-add reclassifying remaining files when committing multiple added files individually
- Parallelize staging of multiple explicit paths instead of a serialized per-path loop
- Make CLI paths relative to the current working directory across all path-printing commands
- Bump `anyhow`, `crossbeam`, and `memmap2` for security advisories

## v0.8.4 (Jun 25th 2026)

### Features

- Add `--dry-run` to `revision commit` and `lock acquire`/`release`
- Run incremental store GC by default; replace `--gc` with `--no-gc` to disable
- Expose the mutable store through the low-level storage C API
- Add `ForwardedRevisionService` gRPC endpoint to forward `BranchCreate` to a remote server
- Carry structured error detail and FFI codes on the `Complete` event
- Show staged renames as moves in diff output
- `lore status` prints paths relative to the current working directory

### Fixes & Improvements

- Fix use-after-free in `write_fragmented` when the chunker future is cancelled
- Fix `LoreArray<T>` dealloc layout mismatch in Drop
- Fix `commit --stats` panic and report real fragment stats
- Fix link contents surfacing as parent adds in file diff
- Fix clone not materializing view-filtered directories with all-excluded children
- Reject malformed `metadata set` args instead of panicking; default branch metadata to current branch
- Reset staged add/remove/update for link nodes
- Propagate dirty to committed ancestor directories on dirty add
- Make dirty add idempotent so a repeat dirty doesn't duplicate the node
- Stage empty dirty-added directories by their own path
- Honor `--dry-run` on branch push
- Rename `[server.replication]` config to `[server.grpc_internal]` (not backwards-compatible)
- Map GRPC storage errors correctly and stop classifying `Unknown`/`EOF` errors as server errors
- Handle thin-client `RevisionDiff`/`RevisionTree` RPCs with a zeroed revision
- Tag linked-repo `RevisionDiff` changes with an indexed partition table
- Standardise HTTP tracing and log levels
- Seed QUIC clients with an initial CWND on regeneration
- Remove redundant `dry_run` field from lock events
- Rename shared store config file to `shared_store.toml` with auto-migration
- Change `stats` flag to `u8` for C ABI consistency
