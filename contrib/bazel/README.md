# lore-rbe

A server for the [Bazel Remote Execution API v2][reapi] whose cache — action results, build inputs,
intermediates and outputs — lives in a Lore store. Bazel talks plain REAPI to it, workers execute
the actions against their own tier of that store, and everything cacheable is stored in and
resolved from a loreserver that any number of machines can share.

A standalone workspace, outside the Lore workspace and built from within `contrib/bazel`: it
reaches the Lore crates by path, restates the workspace's `quinn-proto` patch, and pins its
dependencies in a committed `Cargo.lock`.

[reapi]: https://github.com/bazelbuild/remote-apis

## Architecture

```text
                              bazel
                                |   REAPI v2 over gRPC
                                v
                   +---------------------------+
                   |      lore-rbe-server      |          Lore storage handle
                   |---------------------------|   +--------------------------------+
                   | Capabilities              |-->| local tier: on-disk Lore repo  |
                   | ContentAddressableStorage |   +--------------------------------+
                   | ByteStream                |                  |
                   | ActionCache               |                  v
                   | Execution + Operations    |          +----------------+
                   | WorkerQueue (internal)    |          |   loreserver   |  the shared cache
                   +---------------------------+          +----------------+
                         ^                ^                  ^          ^
                         |  leases and ActionResults only     |          |
             +-----------+                +-----------+      |          |
             |                                        |      |          |
    +------------------+                    +------------------+        |
    | lore-rbe-worker  |        ...         | lore-rbe-worker  |--------+
    | local Lore tier  |                    | local Lore tier  |
    +------------------+                    +------------------+
```

**`lore-rbe-server`** serves everything bazel needs. The CAS and the Action Cache are two key
namespaces in one Lore partition.

**`lore-rbe-worker`** leases an action, materialises its entire input root, runs the command,
stores the outputs, and deletes the scratch directory. It has its own Lore store — a local tier
on disk, tiering to the same shared server — and moves content between that store and the input
root by path, so blob content is never resident in the worker and never passes through the
scheduler.

**The loreserver** is the shared tier, and the only thing that connects the two: a worker sees
what bazel uploaded because both sides tier to it.

### Where the content flows

Bazel uploads inputs to `lore-rbe-server`, which writes them to the shared store. A worker reads
them from its own store, which fetches from the shared one on a miss and keeps what it fetched.
Outputs go back the same way. The scheduler carries leases and `ActionResult`s, not blobs.

The executor-side tier is what makes this worth doing. The input roots of a build's actions
overlap almost entirely, so a worker's local tier serves most of every input root from local
disk, and on a fleet the shared server sends each input to a worker once.

### `LORE_ALLOCATOR=system`

A worker forks to run every action, and it links Lore, which by default reserves a ~344 GB virtual
heap through rpmalloc. Under `vm.overcommit_memory = 0` a `fork()` must reserve commitment
matching the forking process's accountable private mappings, so it fails with `ENOMEM`.

`lore-rbe` sets `LORE_ALLOCATOR=system` before it starts a worker, and the worker refuses to
start without it: the failure it prevents is every action failing to spawn. The variable is read
at the first allocation, before `main`, so it cannot be set from inside the process.

## How the cache maps onto Lore

Bazel's remote cache is two string-keyed blob maps:

| REAPI concept | Key | Value |
|---|---|---|
| CAS | SHA-256 digest of the content | the blob |
| Action Cache | SHA-256 digest of the `Action` message | a serialised `ActionResult` |

Lore's foreign-key storage API has that shape, so each is one pair of calls:

```text
put(key, blob): put_resolved(H(key), blob)   // stores content, then publishes the key
get(key):       get_resolved(H(key))         // resolves the key, returns the content
```

Keys are `H("cas:<hash>:<size>")` and `H("ac:<hash>:<size>")`. The prefix keeps the two
namespaces apart, since an `Action` message is itself a CAS blob and can have the same digest in
both. Everything lives in dedicated non-zero partitions derived from `H("bazel-rbe.v1.…")`, so it
cannot collide with repository data, under one fixed `Context`. The key format is a storage
format: every entry already published is reachable only through it.

### Two partitions

Both hold the same keys over the same content and differ only in lifetime: **build-cache**, which
everything a build produces goes to and which can be capped and garbage-collected, and
**toolchains**, which is durable, shared by every project, and written only by `lore-rbe-index`.

Routing needs no path knowledge:

* `FindMissingBlobs` — present in either counts as present. Both are probed in one `mutable_load`
  with the partition encoded in the item id, so the second costs items, not a round trip.
* Reads — build-cache first, then toolchains. In sequence rather than at once, because two
  concurrent `get_file_resolved` calls would write the same destination path.
* Writes — always build-cache.

Both are keyed by content digest, so a digest that resolves in both holds identical bytes in
both. The routing lives in `rbe-lore`: neither the server nor the worker distinguishes the
partitions, and a worker materialises a seeded compiler by asking for its digest.

Only CAS reads fall through. An `ActionResult` describes one build's outputs and has no meaning
in a partition shared across projects, so the Action Cache is build-cache only.

Tiering is the storage API's: `get_resolved` reads the local tier first and falls back to the
upstream on a miss, and `put_resolved` with `remote_write = 1` publishes upstream, storing content
before the key names it so a key never resolves to something absent. Every operation is batched:
one Lore call carries N items, each with a caller-chosen `id` echoed back on its events, so
`FindMissingBlobs` over a 2000-input action is one call.

### Content that is already a file

The same pair has a path-taking form, which the workers use:

```text
put_file_resolved(H(key), path)   // reads the file, chunking a large one straight off disk
get_file_resolved(H(key), path)   // writes the content, leaf by leaf at its own offset
```

Neither assembles the content in the calling process, so an input root costs scratch space
rather than memory.

* **The empty blob is never stored.** A zero-length file is a retraction to `put_file_resolved`:
  storing one would delete the key it was published under. It is skipped on write and created on
  read, which is also what the REAPI demands of the empty blob.
* **Lore restores content, not permissions.** The executable bit is applied from the `FileNode`.

The worker writes everything through `put_file_resolved`, staging its own `Tree` and `Directory`
messages to files first. `put_resolved` carries a view into caller memory in its arguments, which
has no cross-process representation, so a worker that only names paths can delegate its storage
calls to a Lore service process through `LORE_USE_SERVICE`.

### What each tier keeps

Each process says what its local tier keeps (`LocalCache` in `rbe-lore`). Content is always
published upstream; with the local copy off, Lore keeps only fragment metadata for content the
upstream holds durably, and a read does not keep what it fetched.

* **The scheduler** keeps nothing when the loreserver runs on the same machine
  (`--local-cache false`, which `lore-rbe up` passes): a local copy would write every upload and
  every download to the same disk twice.
* **A worker** keeps the small messages (`Action`, `Command`, `Directory`) and no file content.
  Its staging directory holds every input it has fetched, hardlinked into each input root, and
  an action's outputs are adopted into staging too, so a later action on the same worker links
  them instead of fetching them back.

### Compression

The scheduler offers zstd (`Compressor.ZSTD`) through ByteStream's `compressed-blobs` resource
names and the batch calls; bazel uses it with `--remote_cache_compression`. Digests always name
the uncompressed content. Workers reach Lore directly and never see any of this.

* **Uploads** are decompressed with the output capped at the digest's size and verified against
  the digest. Lore compresses them again as it stores them.
* **Whole-blob downloads are not compressed here.** Lore keeps every fragment of a blob as a zstd
  frame, and frames written one after another are valid zstd data. A compressed read asks Lore for
  the blob's leaves as stored (`Delivery::Zstd`), through `get_resolved`'s `fragments` flag, and
  `zstd_frames.rs` passes a zstd leaf on as it is, so bazel expands what Lore compressed when it
  was written. A leaf Lore kept uncompressed goes out wrapped in raw blocks; a leaf stored with any
  other codec fails the read. A read of part of a blob is cut from the content and compressed here,
  at level 3.
* Compressing, decompressing and hashing anything over 256 KiB runs on the blocking pool.

The `fragments` flag delivers each leaf neither expanded nor verified; bazel checks every digest
it downloads. The scheduler's counters add `wire:` bytes, what crossed the connection.

### Existence without content

`FindMissingBlobs` must answer without reading the content. `mutable_load` resolves a key to its
content hash without reading the blob, but unlike `get_resolved` it does not tier: each item
targets either the local or the remote mutable store. So `exists_many` makes one batched local
call and one more upstream for whatever missed.

Resolving the key upstream is enough to answer present even if the payload is not local yet,
because a later read tiers through and fetches it. Claiming presence for something unreadable
breaks the build, while a false absence costs a re-upload, so every failure path here reports
absent.

## Layout

```text
crates/rbe-proto/     generated REAPI, ByteStream and worker-protocol bindings
crates/rbe-lore/      the Lore-backed blob store, shared by the server and the workers
  src/lib.rs            get/put/exists in memory, get_file/put_file by path, all batched
  src/digest.rs         SHA-256 helpers, including the streaming file digest
  src/zstd_frames.rs    stored fragments as a zstd stream
  src/workspace.rs      the files of a Lore checkout and their recorded digests
crates/rbe-server/    the REAPI endpoint and the scheduler
  src/cas.rs            ContentAddressableStorage, ByteStream, Capabilities
  src/ac.rs             ActionCache, including completeness checking
  src/exec.rs           Execution service, operation state, worker queue
  src/bin/lore_rbe_index.rs
                        lore-rbe-index: publishes content into the durable partition, either
                        uploaded (seed) or copied from a Lore checkout (stamp, publish, xattr)
  examples/cas_dump.rs  fetches blobs by digest and says which REAPI message each one is
crates/rbe-worker/    the executor; owns its own tier of the same store
proto/                the vendored REAPI and googleapis protos (LICENSE-APACHE) and the worker
                      protocol
lore-rbe              lifecycle: build, seed, up, workers, down, wipe-*, status, counters, flags,
                      source-repo, deps-repos
```

## Prerequisites

* Linux. The worker and `lore-rbe-index` use Unix file APIs and Linux extended attributes.
* Rust with edition 2024, and nightly rustfmt for formatting.
* This repository: the workspace depends on the Lore crates and `vendor/quinn-proto` by path.
* For `seed`, `source-repo` and `deps-repos`: bazel, and the `lore` CLI.

## Build

```sh
cd contrib/bazel
./lore-rbe build
```

`build` compiles `lore-rbe-server`, `lore-rbe-worker` and `lore-rbe-index` into `target/release`
here, and `loreserver` in the Lore root. Both run from within their workspace, because cargo
reads `.cargo/config.toml` from the working directory upwards, not from `--manifest-path`, and
the root one sets the rustflags `loreserver` needs (`--cfg tokio_unstable`, `--cfg uuid_unstable`).

`[patch.crates-io] quinn-proto` is restated in this workspace's manifest: `lore-transport` does
not compile against the published quinn-proto, and a patch applies only from the workspace root.

Tests, formatting and lints run from here too:

```sh
cargo test
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
```

## Run

```sh
./lore-rbe up                      # loreserver + rbe-server + 4 workers x 5 slots
./lore-rbe up --workers 8 --slots 4
./lore-rbe up --upstream lore://other-host:41337   # use another loreserver as the shared cache
./lore-rbe status
./lore-rbe down
```

Stores, logs, pid files and scratch go to `.rbe-state` here, or to `LORE_RBE_STATE`. `RBE_PORT`
and `LORE_PORT` move the scheduler and the loreserver off 8980 and 41337; `up` refuses a
`LORE_PORT` held by a process it did not start, since a store it does not manage cannot be wiped.

There is no mode without an upstream. Bazel's uploads reach an executor only through the shared
store, so with nothing shared every action fails on a missing input.

Then point bazel at it. `./lore-rbe flags` prints the flags a build needs:

```sh
bazel build //... --remote_executor=grpc://127.0.0.1:8980 --remote_timeout=3600 \
  --incompatible_strict_action_env --experimental_throttle_remote_action_building=false --jobs=60
```

`--remote_executor` alone is enough to build: one endpoint serves the cache and the executor.
`--incompatible_strict_action_env` is what makes the cache shared: without it bazel copies the
`PATH` of the shell that runs it into every action, and so into every cache key.
`--experimental_throttle_remote_action_building=false` lifts bazel's limit of one remote action per
client core building its input tree and uploading at once, which far from the server caps the
build rate; pair it with a `--jobs` of two to three times the executor slots.

`wipe-local` clears the scheduler's and every worker's local tier and keeps the shared store:
clean executors, populated shared cache. `wipe-all` stops the stack and clears the shared store
too: nothing cached anywhere.

### Seeding the toolchain

For a project built with the hermetic `toolchains_llvm_bootstrapped` toolchain, with the stack
up, publish the toolchain into the durable partition so bazel stops uploading it:

```sh
./lore-rbe seed path/to/project
```

It reads the toolchain from the project's bazel output base, so the project has to have been built
once first. It is needed once per toolchain version.

### Across machines

By default everything binds loopback and the stack is private. `--host` makes it reachable:

```sh
build-host$   ./lore-rbe up --host 10.0.0.1 --workers 2
worker-host$  ./lore-rbe workers --server http://10.0.0.1:8980 --upstream lore://10.0.0.1:41337
```

The worker machine needs the binaries and nothing else: no project, no bazel, no configuration
beyond the two addresses. `--server` is the control plane, leases and `ActionResult`s, and
`--upstream` the data plane, input roots and outputs; only the second carries bulk.

### Sources stored in Lore

Bazel can build a checkout of a Lore repository whose content is already on the loreserver that
holds the build cache, so a clean checkout sends hashes instead of content:

```sh
./lore-rbe deps-repos --project path/to/project --publish toolchain,third-party
./lore-rbe source-repo --project path/to/project --publish --xattr
```

`deps-repos` makes the `toolchain` and `third-party` repositories from the project's bazel output
base, and `source-repo` makes one of the project itself. Each is a new repository in a directory
under `LORE_RBE_SOURCES` (default `~/lore-rbe-sources`), which must not be inside a Lore
workspace: copied, stamped, committed and pushed to `--upstream`, by default this stack's
loreserver.

`lore-rbe-index` does the Lore-specific part:

* `stamp` records each staged file's SHA-256 as Lore file metadata, and `lore-rbe` runs it before
  every commit it makes. Lore addresses content by BLAKE3 and keeps a file's metadata when its
  content changes, so a commit that is not stamped carries stale digests.
* `publish` gives every file of a checkout's revision its CAS key in the durable partition,
  through the checkout's own store, so content the server already holds from the push is linked
  rather than uploaded again.
* `xattr` writes each digest as `user.sha256`, which bazel reads instead of hashing the file when
  started with `--unix_digest_hash_attribute_name=user.sha256`.

### Configuration

Both binaries open a Lore store and share the store flags:

| Flag | Meaning | Default |
|---|---|---|
| `--lore-repo` | local-tier repository path | `cache-repo` / `worker-repo` |
| `--lore-server` | upstream loreserver URL | none |
| `--cache-size` | local tier cap in bytes; enables Lore's GC | 0 (GC off) |
| `--stats-interval` | log the cache counters every N seconds | 0 (off) |

Server only:

| Flag | Meaning | Default |
|---|---|---|
| `--listen` | REAPI and worker queue address | `127.0.0.1:8980` |
| `--verify-ac` | check an Action Cache hit's blobs still exist before serving it | on |
| `--local-cache` | keep a local copy of what passes through, as well as publishing it | on |
| `--log-cas-writes` | append `<hash> <size>` for every uploaded blob to this file | off |

Worker only:

| Flag | Meaning | Default |
|---|---|---|
| `--server` | the scheduler to lease work from | `http://127.0.0.1:8980` |
| `--slots` | concurrent actions in this process | 4 |
| `--scratch` | scratch root; one fresh subdirectory per action | `/tmp/lore-rbe-worker` |
| `--staging-size` | cap on the input staging directory; 0 disables eviction | 32 GiB |
| `--default-timeout` | action timeout when the `Action` sets none, seconds | 900 |
| `--keep-scratch` | keep each action's scratch directory | off |

`LORE_PROF=1` adds one line per cache operation with item counts, hit and miss split, bytes and
latency. `lore://` is QUIC for blobs plus gRPC for metadata; `lores://` validates the server
certificate, `lore://` skips verification.

## What is cached

An action result is written to the Action Cache only when it ran cleanly: exit code 0, no
timeout, and `Action.do_not_cache` unset. A failing action is still a valid result, and bazel
wants its exit code and stderr, but caching it would pin the failure for every other client until
the inputs change.

`--verify-ac` checks that the blobs an Action Cache entry references are still in the CAS before
serving it. Lore's GC can evict a blob while the entry survives, and serving that entry fails the
build with a missing output instead of re-running the action. It costs one batched existence check
per hit.

An Action Cache lookup never fails a build. A read that errored, an entry that does not decode
and an entry whose outputs are gone are all recoverable by executing the action, so all three are
reported as a miss. The `degraded` counter in the server's stats line separates these from plain
absence: a non-zero value means warm builds are running cold.

## Notes

* **`status` is not the success signal** for Lore storage calls; a miss returns `-1`. Branch on
  the per-item `error_code`, where `AddressNotFound` is the only reliable miss.
* **A stable `correlation_id` matters.** Lore keys its storage `SessionPool` on
  `(repository, correlation_id)` and mints a fresh UUID whenever the field is empty, so leaving it
  unset makes every operation miss the pool and pay a `session_start` round trip. One id per
  handle, for its whole life.
* **`local_cache = 1` on writes keeps the payload local**, not only fragment metadata: the write
  path retains it only when `!stored_durable || cache_local`, and a successful upstream write
  makes it durable.
* **The server flushes Lore on shutdown.** Lore's close-time flush is otherwise fire-and-forget,
  and a restart cannot reuse the on-disk repository, so a warm cache comes back cold. The server
  handles SIGTERM as well as SIGINT, and `lore-rbe down` waits for each process to exit.
* **`lore-rbe` writes its own loreserver configuration**, overriding only ports and store paths.
  `lore-server/config/local.toml` references test certificates relative to the repository root.
  With no `[server.quic.certificate]` the QUIC endpoint generates an ephemeral self-signed
  certificate, and with no `[environment]` section the client uses the host and port it dialed.
* **The worker creates the parent directory of every declared output** before running. The REAPI
  makes that the worker's job, and bazel relies on it.
* **Digests are verified on write**, on the batch and ByteStream paths. A wrong digest would
  poison the cache for every client that resolves it.

## Limitations

* **Seeding is coarse.** `seed` publishes whole external repositories, and most of what is in them
  never enters an input root.
* **Input roots are materialised in full.** The executor-local tier makes those reads cheap but
  does not remove them; only materialising what the compiler opens does.
* **Each worker process caches the same content separately.** One Lore service per machine, with
  the workers delegating to it over IPC, would deduplicate it.
* **A worker does not stop an abandoned action.** The scheduler re-queues a lease that goes
  unrenewed and tells the worker on its next heartbeat, but the worker runs the action to the end.
* **No sandboxing.** Actions run unsandboxed on the host.
* **No authentication or multi-tenancy.** `instance_name` is accepted and ignored.
* **The scheduler is FIFO and in memory.** No priorities, no persistence across a restart, no size
  classes, no knowledge of how long an action took before.
* **Bazel's own uploads remain.** With every source file in Lore, a cold build still uploads what
  bazel generates during the build: link parameter files and the per-action REAPI messages.
