# Release Notes

Release notes for the open source Lore project. Releases before v0.8.4 predate this file; see the
[GitHub releases](https://github.com/EpicGames/lore/releases) page for the published record.

## Nightly

### Breaking changes

- `lore-server`: with authorization configured, operations that change a partition check for the `write` permission.
  Any permission on a partition still allows reading it, but no longer allows writing. Servers without authorization
  configured allow every caller to read, write and administer repositories
- `lore-server`: changing branch protection, through branch protect and unprotect or by setting the `protect` key in
  branch metadata, needs `owner` or `admin` on the repository, or `write` for its creator. Previously anyone with
  access to the repository could set the branch protection rules
- `lore-server`: vending a presigned URL needs the `presign` permission on the repository instead of the
  `is_service_account` token claim. Servers without authorization configured still allow every caller to vend
- Server hooks: `Hook::pre_handler` is now `async`, so a pre-handler can call other services without blocking a worker
  thread. Custom hooks must make their `pre_handler` async and rebuild. A pre-handler is cancelled at its timeout
  (1 second, or the hook's own `pre_handler_timeout`), and a timeout now fails the request with `UNAVAILABLE` instead of
  `INTERNAL`
- A client connected to a `lores://` remote refuses to connect, failing with `NotSupported`, if the server advertises a
  storage, revision, lock, repository, or notification endpoint whose scheme doesn't authenticate the server, such as
  `lore://`. It used to connect to that endpoint without validating its certificate, and for gRPC without TLS. A
  server that advertises these endpoints must give them a scheme that authenticates the server, such as `lores://`

### Features

- `lore-server`: forward the lock service's `Query` to a remote Lore server, opt-in via `lock_query` under
  `[server.grpc_public_services.forwarded_requests.enabled_rpcs]`. The peer serves it only when it has a lock store
- QUIC connections to replication peers configured under `[immutable_store.composite.replica_factory]` start with a
  1 MiB initial congestion window, set with `initial_cwnd_bytes`. Values below 2400 bytes are rejected at startup. A
  periodically refreshed connection starts at no less than this window

### Fixes & Improvements

- A revision named by number, such as `<branch>@<number>`, resolves from the first page the remote returns for it,
  rather than walking the branch's history back from its latest revision one page at a time
- Following a branch's history through the remote no longer takes the revisions a page holds above the one it was
  asked for as that revision's ancestors. A page the server serves from a cached segment holds them, and finding the
  revision two branches share could answer with one of them, newer than the one they really share
- On Windows, a Lore service started while another serves the same socket exits without serving, as it does on Linux
  and macOS. It used to take the socket over and leave the first service running unreachable, holding its SWFS mounts
  until killed, so commands that each started a service at the same moment could leave one behind per extra command
- A branch push reads each mutable store key once, loads the branch alongside the pushed revision, and checks the
  branch name while it verifies the revision's fragments rather than before, so a push spends fewer store round trips
  before it moves the branch. A push the server refuses because the branch is protected no longer reinstates the name
  of a deleted branch first
- The deprecated `urc.rpc` `BranchPush` RPC treats a missing or deleted branch as `lore.revision.v1` `BranchPush`
  does: a branch without metadata is `NOT_FOUND` rather than an internal error, and a push to a deleted branch
  reinstates its name, or fails with `ALREADY_EXISTS` when another branch holds the name, rather than failing with
  `NOT_FOUND`

## v0.12.0 (Oct 26th 2026) [#1966]

### Breaking changes

### Features

- `lore service run` runs the Lore service in the current process, returning once it stops, whether by
  `lore service stop` or by `lore::service::quit` from within the same process. `lore::service::quit` asks a service
  that `lore::service::run` started in this process to shut down cleanly, and does nothing when none runs there. Neither
  has a C API equivalent: there is no `lore_service_run` or `lore_service_quit`, so they're available from Rust only.
  `lore service stop` (C API: `lore_service_stop`) is the way to stop a service from another process.
- An OIDC login whose access token has expired is renewed with its refresh token (OAuth 2.0 refresh grant, RFC 6749
  §6) on the next operation, without a new `lore auth login`. The provider's rotated refresh token replaces the used
  one, and a refresh token the provider refuses is dropped, so the next operation asks for a new login. A client
  credentials login stores no refresh token
- `lore auth logout` (C API: `lore_auth_logout`) revokes an OIDC login's refresh token at the provider (OAuth 2.0
  token revocation, RFC 7009) if the provider advertises a revocation endpoint. A failed revocation, such as at an
  unreachable provider, now fails the logout, after the login is removed from this machine

### Fixes & Improvements

- Clone and sync continue materializing available files after an individual write fails, restoring
  the behavior established by UCS-14675. Failures are reported and the command still exits nonzero.

- `lore file obliterate` warns before deleting content needed by an active branch’s latest revision, including
  shared child fragments. Incomplete checks report unknown impact; the administrative deletion still proceeds.

- `lore repository verify` fails where it can't read part of the local store, where it passed over it, and checks and
  heals every fragment a flush in the same process moves to other index buckets meanwhile
- `lore stage` of several paths under a directory missing from the working directory stages a delete of only those
  paths, where it staged a delete of the whole directory
- `lore branch switch <branch> <revision>` takes a revision that names no branch, such as `1234` or `@1234`, on
  `<branch>`, where it took it on the current branch and refused it as not on the target branch
- `--dry-run` writes nothing to the working tree. `lore file reset --dry-run` restored, deleted and purged files as a
  reset without it does, and other commands could write files the same way
- `--dry-run` changes no branch, revision, staged or layer state and creates no repository. `lore branch create`,
  `branch reset`, `branch archive`, `revision amend`, `unstage`, `file dependency add`, `layer add` and
  `repository create` made their changes with `--dry-run` as without it, and `repository create --force --dry-run`
  removed the existing repository
- `lore repository clone --dry-run` into an existing directory leaves nothing in it, where it left a `.lore` directory.
  `repository clone --force --dry-run` over an existing repository fails instead of replacing it, and
  `repository clone --dry-run --vfs swfs` fails instead of mounting the repository
- `lore shared-store create --dry-run` creates and registers no shared store, and with `--force` keeps the existing one.
  `repository clone --dry-run --use-shared-store` fails where it would create the shared store, and a dry run no
  longer moves a shared store's legacy `global.toml` config to `shared_store.toml`
- `--dry-run` writes no missing `.lore/instance` file and saves no identity or remote URL to `.lore/config.toml`
- `lore unstage --dry-run` and `reset --dry-run` of a staged link addition keep the link's files, where they removed
  them, and `lore file write --dry-run` writes no output outside the repository
- `--dry-run` changes nothing on the remote. `lore branch create --dry-run` created the branch in linked and layer
  repositories and with `--force` deleted the existing branch on the remote, `link add` and `layer add` created the
  current branch in the linked or layer repository, `branch metadata set` and `repository metadata set` stored their
  values, `branch merge into` pushed the merge to the target branch, and `commit` uploaded the commit's content
- `lore shared-store set-use-automatically`, `service set-use-automatically` and `service set-executable` change no
  setting with `--dry-run`, and the service setters say what they would set
- `lore file obliterate --dry-run` removes no content, where it obliterated the content in the local store and on the
  remote
- `lore repository gc --dry-run` collects no garbage, where it evicted and compacted the local store, and
  `repository verify --heal --dry-run` heals nothing, reporting what it finds as `repository verify` does
- `lore auth login --dry-run` authenticates and stores no token, and `auth logout --dry-run` and `auth clear --dry-run`
  remove no token, where all three changed the token store
- Processes running commands on one repository or shared store at once, such as several `lore history`, no longer wait
  on each other forever. A command whose process still had the immutable store open from the previous command, and not
  the mutable store, took the mutable store's lock before the immutable store's, against a process holding the immutable
  store's lock and waiting for the mutable store's. Every process using the store needs this version: an older one still
  takes the two locks in that order
- TLS connections refuse a server certificate issued through one of the eMudhra intermediate CAs whose private keys can
  be derived from their public keys (RUSTSEC-2026-0382), whichever built-in, operating system or custom root its chain
  ends in, where they accepted it. The built-in roots no longer include emSign ECC Root CA - G3, emSign Root CA - C1 or
  emSign ECC Root CA - C3
- TLS connections abort on a TLS 1.3 handshake message sent at the wrong encryption level, where they accepted it
  (RUSTSEC-2026-0285). A forged ServerHello no longer makes later connections to the same server offer only a classical
  key share, which a server could accept without post-quantum key exchange (RUSTSEC-2026-0349). Debug output of a
  client session leaves out its secrets (RUSTSEC-2026-0346)
- `lore stage --case keep` of several paths under a directory spelled in another case than the repository's stages them
  as modified under the repository's case, where on a case-sensitive file system it staged them as deleted

## v0.11.0 (Oct 21st 2026) [#1892]

### Breaking changes

- C API: the new `LORE_EVENT_FILE_AUDIT` event adds a member to `lore_event_t`, so C consumers must rebuild against the
  new `lore.h`
- `[environment.endpoint] user_url` now names a `lore.user.v1.UserService` (`UserGet`, `UserFind`), replacing the
  `UrcAuthApi` `GetUserInfo` and `GetUserId` calls. Without `user_url`, clients ask `auth_url` over `UrcAuthApi` as
  before. With `user_url`, the server also lists repositories there (`PartitionList`) by default, through the new
  `[server.auth] repository_catalog = "user_service"` mode, and registers the repositories it creates and deletes there
  (`PartitionAdd`, `PartitionRemove`) instead of calling `RebacApi` at `auth_url`
- A client and the Lore service it relays to must come from the same build: the message protocol changed (version 2),
  and mismatched builds refuse each other's messages. Restart the service after upgrading
- Data is verified by the trust of its source ([ADR-00023](developing/decisions/00023-data-verification-and-trust.md)):
  reads from the local store no longer verify hashes, and data fetched from a remote is verified on receipt unless the
  remote is trusted (`--trust-remote`) or a C API reader takes fragments as stored (`fragments`). C API: `skip_verify`
  on `lore_storage_open_args_t` is now an alias of `trust_remote` for its handle, so it skips verifying fetched data
  only
- C API: `lore_branch_merge_start_args_t` gains `link_paths` and `link_messages`, changing its size; rebuild against the
  new `lore.h`. A zero-initialized struct keeps today's behavior
- C API: `lore_auth_login_with_token_args_t` gains `client_id` and `client_secret`, changing its size; rebuild against
  the new `lore.h`. A zero-initialized struct keeps today's behavior
- The server refuses to start if `[server.auth] resource_id_template` or `[server.auth.oidc]`
  `resource_template`/`scope_template` lacks `{id}`, as such a template let one grant match every partition. The error
  names the setting

### Features

- `lore file audit <path>` is a new command that shows, for each line of a file, the revision that last changed it,
  grouped into ranges of consecutive lines. The C API offers the same query as `lore_file_audit` and
  `lore_file_audit_async`
- `--trust-remote` (C API: `trust_remote` on `lore_global_args_t`) skips hash verification of data fetched from the
  remote, keeping only a size check, for machines inside the remote's own infrastructure where performance comes first,
  such as build servers. The field sits in the struct's padding, so callers must zero the whole struct, padding
  included, or stray bytes turn it on
- C API: `fragments` on `lore_storage_get_resolved_item_t` delivers content piece by piece as
  `LORE_EVENT_STORAGE_GET_FRAGMENT`, each with its `lore_fragment_t` and payload as stored, instead of
  `LORE_EVENT_STORAGE_GET_DATA`. The reader decodes and verifies the payloads itself, and can keep them compressed as
  stored instead of recompressing. Zero-initialized items read as before
- `lore auth login` against a server with an OIDC issuer logs in with the OAuth 2.0 device authorization grant (RFC
  8628), requesting the server's default scopes and `offline_access`: it opens the verification URL in the browser, or
  prints it with `--no-browser`, shows the code to confirm, and stores the access and refresh tokens.
  `--token-type <TYPE> --token <TOKEN>` logs in with an existing token through RFC 8693 token exchange, passing the type
  to the issuer verbatim. Partition operations, repository metadata get/set and repository delete send the access token,
  or a per-partition token from token exchange when the server advertises `resource_template` or `scope_template`. A
  token exchange issuer on another host gets the login token only if the token's `root_domains` claim, or else `aud`,
  names that host. ID tokens are never stored or sent. The refresh grant isn't implemented yet
- `lore auth login --client-id <ID>` against a server with an OIDC issuer logs in as a confidential client registered
  at the provider, with the OAuth 2.0 client credentials grant (RFC 6749 §4.4), for CI. The secret comes from
  `LORE_CLIENT_SECRET`, or from `--client-secret`. The client authenticates with HTTP Basic, or in the form when the
  provider's discovery document lists only `client_secret_post`. No scope is requested. The C API takes the same login
  through `client_id` and `client_secret` on `lore_auth_login_with_token_args_t`
- `--token`, `--identity-token` and `--access-token` fall back to `LORE_TOKEN`, `LORE_IDENTITY_TOKEN` and
  `LORE_ACCESS_TOKEN` when not given. A secret passed as a flag value, these three or `--client-secret`, now prints a
  warning, since other users can read it in the process table and the shell history keeps it
- The user identity is read from the token by the same rule on client and server: the `identity` claim (or
  `https://lore.org/claims/identity`), else `sub`. An issuer whose `sub` isn't a usable identity maps one into
  `identity`, such as Entra's `oid`. Tokens with an empty or non-string identity are refused. The
  `[server.auth] identity_claim` setting, and its advertisement in the environment response, are removed
- `lore branch merge` and `lore branch merge start` take `--link-message <PATH> <MESSAGE>`, as `lore commit` does, to
  give a linked repository its own message in an auto-committed merge. A path that isn't a link fails the merge before
  it starts
- `lore clone --offline` or `--cache` prepares the clone for working offline: staging and committing then work without a
  connection, and the clone fails if it can't fetch what they need. The clone itself still needs the remote. Without
  either flag, the first stage or commit fetches that data from the remote
- `lore service status` reports the status of the running Lore service, if any
- `LORE_STORE_RETAIN=1` (or `true`) keeps a process's stores loaded between repository calls and after
  `lore_storage_close`, without holding their locks, for `store_keep_alive_seconds` (default 10), so calls in that
  window skip reloading unless another process changed the store. While it's set, don't share a store directory with
  processes of an earlier build: their changes go unnoticed and stale data is served. Without it, `store_keep_alive`
  keeps stores loaded and locked for the window
- Server: `[server.http] health_only = true` serves only `/health_check`, for deployments that don't serve repository
  HTTP routes
- `lore-server` metrics: gRPC requests record `rpc.server.handler.duration`, which leaves out the time a client takes to
  send its request and work done before the body is read, such as authorization, unlike `rpc.server.duration` and
  `http.server.request.duration` (bidirectional streams record the same value in all three). `BranchPush` records its
  fragment verification load per push under `lore.branch_push` (`fragment_lookups`, `peak_fragment_lookups_in_flight`,
  `collected_addresses`, `concurrent_query_batches`), with the same counts in debug logs
- `contrib/bazel/`: a Bazel remote execution server on Lore storage, as a standalone workspace built from within that
  directory: a Remote Execution API v2 endpoint and scheduler keeping the action cache and content-addressable storage
  in Lore partitions, executors that share content through a loreserver, and a script that builds, runs and stops them

### Fixes & Improvements

- Working tree: `lore sync --reset` replaces a directory with a file at the same path instead of failing with "Is a
  directory". Syncs, branch switches, resets, merges and view changes send `LORE_EVENT_REVISION_SYNC_FILE` for each
  deleted file and directory, so callers tracking the working tree, such as editors, no longer keep deleted paths. Files
  and directories named `.urc` or `.lore`, in any case, are ignored by every command that reads the tree, including the
  virtual filesystem
- `lore commit` and `lore clone` report the error that stopped them instead of `Recursion task failed`. A failed clone
  no longer prints `Clone complete`, and the C API sends `LORE_EVENT_REPOSITORY_CLONE_END` only on success;
  `LORE_EVENT_COMPLETE` reports the outcome either way
- `lore status`: `--scan` given many paths is faster, scanning them in parallel instead of one after another. Files are
  listed in path order, stable from run to run. `lore status` and `lore stage <directory>` without `--scan` no longer
  miss a modified file after a scan that dropped a reverted add. A file saved again at the same size right after a
  command read it no longer shows as unchanged
- Merges of divergent history, such as a sync when the local and remote branch both have new revisions, or a diverged
  link in a same-branch `lore branch merge`: `lore file history` attributes the remote's changes to the right files, a
  file both sides added keeps the remote's addition, and earlier remote changes are no longer lost. Committing such a
  merge no longer fails in some cases where the remote side added files, and `lore file history` lists files added
  locally before it. The merge keeps the remote's latest revision as its first parent when anything is changed on top
  before committing, including under `lore commit --force`, and once committed `lore status` reports the branch ahead of
  the remote instead of diverged
- Layers: `lore commit` with changes staged in a layer commits the layer even when the parent tree holds only unstaged
  dirty changes, which stay dirty, instead of failing with `Nothing staged for commit`. A `lore status --scan` that
  finds a parent tree change while only a layer is staged no longer makes the next commit fail with
  `Staged state has stale parent` or reuse the current revision's metadata. `lore file dependency add`/`remove`,
  `lore file metadata clear` with nothing staged, and `lore branch merge abort --ignore-links` no longer copy the
  current revision's metadata, such as its reviewer, onto the next revision
- Merge conflicts: a merge no longer drops one branch's change to a file without reporting a conflict, in some cases
  where the other branch added that file after another was deleted. Resolving conflicts to one side
  (`lore branch merge resolve mine`/`theirs`, and the cherry-pick and revert resolves) no longer uses memory in
  proportion to the size of the tree
- Links: committing through a link pinned at a merge revision no longer records a spurious merge in the linked
  repository, and staging changes inside a nested link more than once no longer pushes the intermediate staged states as
  revisions. `lore commit --link-message` accepts any spelling of the link path, such as `./vendor/lib/`; a C API commit
  naming a path that isn't a link fails as invalid arguments, and a message for a link with nothing staged is ignored
  instead of refused
- `lore dirty` inside a link, or on its mount, marks the change in the linked repository, where it recorded nothing, so
  `lore status` reports it without a scan and staging picks it up. The marks survive commits and branch switches,
  `lore status --check-dirty` saves what it verifies inside links, and a link holding only dirty marks is committed at
  the revision it's pinned to. `lore file dirty move` and `copy` refuse paths inside a link
- Lore service: commands it runs use the caller's configuration and credentials (`LORE_GLOBAL_PATH`, `LORE_AUTH_PATH`)
  instead of the service's. `lore_storage_put`, `lore_storage_put_resolved`, `lore_storage_get` and
  `lore_storage_get_resolved` now work through the service. A shared store or instance it can't read at startup is
  logged and skipped, instead of leaving no SWFS instance mounted or creatable. On Linux and macOS, two services started
  at once no longer both run, so the commands that started them reach the same service
- Auth: the CLI sends tokens only to hosts named in their `root_domains`, `https://lore.org/claims/root_domains` or
  `aud` claims, and no longer to the `iss` host except for token exchange. With `[server.auth] resource_claim` set, a
  repository lookup by name or id answers any verified caller with the repository's id, name and metadata hash, so
  `lore repository clone`, `lore link add`, `lore layer add`, `lore repository info` and `lore repository delete` no
  longer fail as not found before the caller holds a grant; reading metadata or contents still needs one. Servers
  connect to a `ucs-auth://` `auth_url` over TLS, as clients do
- Server write checks: pushing to a protected branch requires the `push-protected` action, and the `is_service_account`
  token claim no longer bypasses protection, so grant `push-protected` to mirroring accounts first. The generic
  `MutableStore` and `MutableCompareAndSwap` requests, including C API `lore_storage` calls with `remote` set and
  servers with `mutable_store.mode = "remote"`, can no longer write repository metadata, branch metadata or branch
  latest pointers, which skipped the server's checks; use `RepositoryMetadataSet`, `BranchMetadataSet` and `BranchPush`.
  The refusal is `InvalidArgument` over gRPC and `Failed` over QUIC. C API calls on a handle's local store are unchanged
- `RevisionList`: listing from a revision above the branch's latest returns no forward cursor instead of crashing the
  server, and paging forward through older history on a deep branch no longer times out with `Cancelled`
- Pushing and committing: some large files, depending on how they split into fragments, no longer fail to commit or
  push. A `lore push` retried after an interrupted push, and a gRPC push copying the same content to several files, no
  longer fail on every retry with a missing fragment
- Verification: caches, prefetches and `lore repository store immutable query` verify fetched data unless the remote is
  trusted, and data that fails is fetched again once after the server is asked to repair it, then neither returned nor
  stored (the query logs it and skips it). Corrupt local data is fetched again and replaced instead of crashing an
  unverified read, and a corrupt local root is replaced once instead of fetched again on every read. A fragment list
  naming itself or an ancestor fails caching, and local stores refuse to store fragments with inconsistent flags or
  sizes. Pushing content held in the local store no longer decodes and re-hashes it first
- Reads: streaming reads and reads into files enforce the content size limit as other reads do, and oversized remote
  content is refused before it's stored locally. A corrupt fragment list with out-of-order offsets fails the read
  instead of returning the wrong data. Reads of fragmented content do less redundant work: one local lookup instead of
  two on a miss with no remote, a busy local store retried before going to the remote, and no extra copy of each piece
- Fetching: concurrent reads of the same missing content, including identical content in different files, download it
  once, and a read of content being stored meanwhile waits and reads it locally. Caching runs at most 512 fetches at
  once per remote connection (`LORE_CACHE_GETS` sets another limit, `0` none), so reads a command needs right away don't
  queue behind a long cache. Caching shares downloads with reads, fetches what a local fragment list is missing and
  replaces a corrupt local copy, and what it caches can be evicted from the client store like other fetched data
- Servers using a `remote` store verify data they fetch from the backend, not only its size, and ask the backend to
  repair data that fails. Data in a codec the server is built without, such as Oodle, is refused instead of passed on
  unchecked. The server's HTTP content and presigned redeem endpoints treat data that still fails, or an unreachable
  backend, as missing. Servers also size-check data from replicas and replicating peers, and no longer crash on a
  truncated replica response
- Oodle: the local store's Oodle migration finishes sooner, leaving Oodle data the remote also holds in place. A build
  without Oodle that finds such data replaces it with the remote's re-encoded copy, and reports `NotSupported` instead
  of missing data where there's no remote copy
- Commands start faster, as opening a repository lists each store's index once instead of checking for every possible
  index group, twice. `lore repository create` syncs to disk only with `--sync-data`, as other commands do
- Fix a deadlock between processes using one repository, such as the Lore service and a storage handle. Stores are now
  locked only while in use, so an idle handle or kept-alive store no longer blocks other processes or holds its files
  open, and background work, such as the flush when a handle closes, no longer leaves other operations on the store
  running without its lock
- Local stores no longer lose data after transient I/O errors: a store directory or file that can't be read, as after an
  I/O error or running out of file descriptors, fails the command for a later retry, where it was treated as never
  written and index entries and local-only data were discarded. A listing of the local mutable store fails instead of
  missing entries, and after failing to open a new data file, the next write retries instead of every write failing with
  `No more packfiles`
- A listing of the local mutable store no longer misses entries that a flush in the same process moves to other index
  buckets meanwhile
- A local store whose `version` file can't be read fails to open, where it was marked for migration again on every open,
  making every process using it reload it at each use. A store affected by this recovers at its next command connected
  to the remote; until then, commands without a connection fail with "Unable to upgrade local store to current client,
  need a connection to server"
- Local store flushes: entries a flush reported written are no longer lost when a concurrent index write fails, the
  process stops mid-write, or a mutable store's background flush fails. Entries removed by obliterate, eviction or
  compaction no longer come back after a reopen, notably on servers, serving another file's data. An entry a machine
  failure left pointing past the end of its data is detected and fetched again instead of reading as other content, and
  a data file whose sync fails takes no more writes
- Background index flushes: local immutable stores schedule one after every change, including marking content durable
  and copying it to another partition, and the Lore service's disk stores flush 30 seconds after any write, including
  content it fetched (non-durable writes previously flushed after 5). A crash before the next explicit flush lost these
  changes
- Local stores: a flush or obliterate cancelled part way, as when a command stops waiting for it, no longer loses
  entries or lists a key twice after a reopen, and no longer leaves the store locked until its next use. A cancelled
  write no longer causes needless reloads or a "No such file or directory" error on the next write
- Local stores: an index reorganization that fails or is interrupted no longer serves other content's data, brings back
  obliterated content, makes most entries unfindable after a reopen, or leaves temporary files behind. One that failed
  late is completed by the next flush
- Obliterate in local stores: the obliteration is saved to disk before the data is zeroed and waits for reads in
  progress, so a crash or concurrent read no longer sees zeroed or partly zeroed data, and a failed obliterate leaves
  the content intact. Obliterated content no longer comes back empty when stored again under another context or
  re-encoded by the Oodle migration, and is restored only by storing it again with its payload. Obliterating content
  only partly held locally fails instead of leaving some of it readable. `LORE_EVENT_STORAGE_GET_FRAGMENT` flags report
  correctly whether the local store holds the payload
- Compaction does far less disk work: it rewrites a data file only once enough of its space has gone unused, copies each
  payload once, and no longer stalls server stores for minutes by rewriting the whole index for each file. Client stores
  count local-only data toward `max_size` and `max_capacity` but never evict it. `lore repository gc` loads the full
  index before evicting and no longer swaps `max_size` (compaction) and `max_capacity` (eviction)
- Storage sessions: a process making many storage calls to one remote no longer has every later call refused until it
  restarts, as handles reuse one server session per partition instead of leaving one open per call, and failed or
  cancelled connection setups no longer leave sessions open. When the server refuses a storage session as unauthorized,
  as after revoked grants, the client discards its cached authorization and retries once with a fresh one, where every
  storage call failed until it expired, in the Lore service even after logging in again
- Reconnects: storage handles and servers' remote stores reconnect after a failed connection instead of failing every
  call until reopened or restarted. A failed reconnect is retried with a backoff of up to 30 seconds, calls failing at
  once meanwhile, though a credential refusal doesn't start the backoff. After a QUIC reconnect, commands no longer fail
  on sessions from the old connection, and a stream that stopped carrying requests reconnects instead of failing or
  hanging every later request on it. A request cancelled while being sent no longer corrupts the next request on its
  stream. Reads and uploads use all the connections `--max-connections` allows instead of one
- QUIC: the vendored `quinn-proto` moves to 0.11.19, fixing CVE-2026-25800 (remote memory exhaustion, no credentials
  needed) and other upstream memory-exhaustion, stream-limit and Retry-token issues, a crash a peer could trigger with
  its ACK settings, and path MTU detection that dropped bulk transfers to the minimum packet size
- QUIC throughput: congestion control no longer over-grows while flow control holds the sender back, or loses throughput
  for the rest of a connection after one low round-trip sample, and a complete message no longer waits behind an earlier
  incomplete one on the same stream. The client asks for a larger UDP receive buffer; on Linux, set `net.core.rmem_max`
  to `524288` or more to allow it
- C API: a call reading into `data_out` that stops part way no longer writes the caller's buffer after the call
  completes. `data_out` must stay valid until completion, and `lore_shutdown` is safe only once every call has
  completed, as `lore.h` now documents, correcting its earlier claim that argument memory could be freed once an
  asynchronous call returned

## v0.10.1 (Oct 5th 2026) [#1475]

### Breaking changes

- `lore.thin_client.v1`: `DiffChange.content_from` / `content_to` and `ContentDiffRequest.address_from` / `address_to` /
  `address_base` become a `lore.model.v1.Address`, the form `TreeNode.address` already carried, and move to field
  numbers 10 and 11, and 8, 9 and 10; the hash-only `bytes` fields they replace (5 and 6, and 1 through 3) are reserved.
  A committed file's content resolves under a context generated per file, so a consumer keying content and metadata
  lookups on `hash-context` found nothing for any file in a diff, and had nowhere to put the context when asking
  `ContentDiff` for the file's text. A side that does not exist reports the field unset: an ADD has no from side and a
  DELETE no to side. A link pin change addresses each of its revisions under the linked repository. A consumer built
  against the old fields reads the new ones as absent rather than as a hash, so rebuild against the new bindings
- C API: every storage `*_ITEM_COMPLETE` event replaces `error_code` with an `error` detail carrying the failure's own
  FFI code, message and trace, as `Complete` already did, and the call's `status` becomes the dominant item failure's
  code. Re-check any branch on a per-item code: a missing payload reports `PayloadNotFound` (81) rather than
  `AddressNotFound` (80), a buffer short of the content reports `Oversized` (118) rather than `InvalidArguments` (3),
  and most failures previously reported as `Internal` now report their own code. The structs grow and are no longer
  trivially copyable, so copy the detail's strings before the callback returns and rebuild against the new `lore.h`. The
  revision-tree per-item events are unchanged: they still carry `error_code` as a `lore_error_code_t`, so the five-value
  folding still applies to them, and moving them to a detail will need a second rebuild in a later release
- C API: an empty `lore_string_t` the library emits now carries a NULL `string` pointer rather than a pointer to a
  zero-length NUL-terminated buffer, which is what `lore_string_t` has always documented and what an empty
  `lore_array_t` already answers. A consumer that read `string` without first checking `length` — `strlen(s.string)`,
  `printf("%s", s.string)` — must check `length`, or treat NULL as the empty string. This applies to every empty string
  on every event and every verb, not one field
- C API: `lore_revision_sync_args_t` appends `view`, a path to a view filter file, empty to keep the view the instance
  holds. Rebuild against the new `lore.h`
- Build: the C library is built by the new `lore-capi` package. `cargo build -p lore` builds only the Rust library;
  build `-p lore-capi` for `liblore.so` and `liblore.a` (`liblore.dylib`, or `lore.dll` and `lore.lib` on Windows) and
  `lore.h`, under the same names and paths as before. Building the CLI no longer compiles and links the C library
- Build: the build version is no longer compiled in from `LORE_BUILD_VERSION_NAME`, and a build no longer runs
  `lore revision info` to name itself. Every build reports `<package version>+local` until the new `lore-stamp` tool
  writes a build name into the finished binaries and libraries, which then report `<package version>+<name>` (see
  `CONTRIBUTING.md`). A pipeline that sets `LORE_BUILD_VERSION_NAME` must run `lore-stamp --build <name>` after the
  build and before signing instead. A changing build number no longer invalidates the build cache for every crate
- `lore-server`: a `[server.auth]` without `[environment.endpoint] auth_url`, which authorizes from token claims,
  refuses to start without `[server.auth.oidc]`, since clients have no other way to log in. Add the table with the
  provider's public `client_id`. Tier 2 (`resource_claim` set) also requires `resource_template` or `scope_template` in
  it, and Tier 1 refuses both
- `lore-server`: the server-to-server replication protocol moves `ImmutableCopy` to opcode 21, its request carrying a
  byte of behavior flags where it carried a durability byte alone. Opcode 19 is left unused, so a server on one side of
  the change answers a peer on the other with `UnknownOpcode` rather than reading the request under the wrong shape.
  Copies between servers of different builds fail while a deployment holds both, so upgrade the servers of a replication
  set together. The client-facing storage protocol is unchanged

### Features

- C API: `lore_revision_cherry_pick` and `lore_revision_cherry_pick_async` cherry-pick a revision onto the current
  branch, as `lore revision cherry-pick` does
- C API: `lore_branch_latest_list` and `lore_branch_latest_list_async` list the revisions a branch LATEST has held, most
  recent first, and relay to the Lore service when one is in use. `lore branch latest list` takes the same path, so with
  the service in use it no longer waits indefinitely on the local store the service holds
- `lore sync --view <file>` changes the view filter an instance materializes its working files under, carrying the
  working tree to what the new view holds rather than re-cloning
- `lore-server`: a copy the durable store accepts is read back and recorded, with its content, in the local store and at
  the write replicas, so a later read of the destination is answerable without a round trip to the durable store. The
  read and the writes are detached, so they do not hold up the copy.
  `[immutable_store.composite] record_copy_out_of_band = false` turns it off for a deployment where the extra read and
  write per copy is not worth that, leaving the durable store the only holder of the destination
- `lore-server`: the disk space available to the local stores is checked on a timer and a warning is logged once it
  falls below a threshold. `[server.local_store_monitor]` carries `check_interval_seconds` (default 30) and
  `low_space_threshold_bytes` (default 10 GiB); an interval of 0 turns the check off. Every local store the server
  writes at is watched, the immutable store and the mutable store among them, and the reading is taken per volume:
  stores sharing a filesystem draw one warning naming them all, rather than one warning each. A server whose stores are
  not local is not checked
- `lore file reset --last-merged-from <branch>` takes `--mine` or `--theirs`, restoring the version the reset branch
  held going into that merge or the version the named branch brought in, rather than the version the merge's conflicts
  were resolved with. A merge revision holds the resolved content, so the side that lost a resolution was otherwise
  unreachable; where no merge from the branch is found the branch point is restored as before, whichever flag is given.
  C API: `lore_file_reset_to_last_merged_args_t` appends a `merge_side` (`0` = resolved, `1` = self, `2` = other) in the
  struct's tail padding, so its size and its other fields' offsets are unchanged and a zero-initialized struct keeps
  today's behavior
- `[environment.endpoint] user_url` advertises a user directory that's decoupled from the auth service. Clients resolve
  user IDs to display names using this service's `GetUserInfo` and `GetUserId` operations. For backward compatibility,
  `auth_url` is used as the user service URL, if `user_url` is not defined. Client-side, the lookup moves off the
  `Authentication` trait onto a `UserService` trait
- `lore-server`: `lore repository list` answers through a `RepositoryCatalog` implementation. The implementation is
  selected based on server configuration. A `UrcAuthApi` deployment asks `LookupUserPermissions` as before, a server
  with no `[server.auth]` lists everything it holds. A deployment authorizing from token claims uses a catalog
  implementation based `repository_catalog` and `repository_catalog_url` configuration.
- C API: `lore_repository_delete` and `lore_repository_delete_async` delete a remote repository, and are carried out by
  the Lore service when one is in use. `lore repository delete` is relayed the same way, where it previously always ran
  in the calling process
- `lore-proto`: introduces `lore.user.v1.UserService`, the user directory in Lore's own terms: `UserGet` resolves user
  IDs to users, `UserFind` resolves a name to a user, and `PartitionList` streams the partitions the caller is allowed
  to see. These take over what `GetUserInfo`, `GetUserId` and `LookupUserPermissions` did in `UrcAuthApi`. Only the pure
  OIDC-compliant authn/authz operations are left in `UrcAuthApi` so that we can use OIDC authorization as a drop-in
  replacement for the Auth API.
- `lore-server`: the new `[server.auth.oidc]` table configures OIDC info shown to clients, through the new `oidc` field
  of `lore.environment.v1.Environment`
- `lore-server`: `[server.auth] jwt_typ` lists the accepted JWT `typ` header values, as a string or an array. When set,
  a token whose header carries no `typ`, or a `typ` that doesn't match any of the entries, is rejected. If unset, the
  token `typ` value is not checked, as before
- C API: `lore_revision_bisect` and `lore_revision_bisect_async` take one step of a bisect between two revisions,
  synchronizing the working directory to the revision halfway between them, and relay to the Lore service when one is in
  use. `lore revision bisect` takes the same path, so with the service in use it no longer waits indefinitely on the
  local store the service holds
- C API: `lore_shared_store_list` and `lore_shared_store_list_async` list the registered shared stores and, when asked,
  the instances using each, as `lore shared-store list` does

### Fixes & Improvements

- `lore link list --staged`, and an interactive `lore commit` listing the links it asks a message for, run in the Lore
  service when one is in use, as other commands do, rather than opening the repository in the client, where they waited
  on the lock of a service holding it. C API: `lore_link_list_staged` and `lore_link_list_staged_async` expose the
  listing
- `lore commit` stops with the failure of listing the links or layers its per-link and per-layer messages are resolved
  against, such as a Lore service in use that cannot be reached, where it rejected a `--link-message` or
  `--layer-message` path as matching nothing, or committed without asking for those messages
- `lore repository instance prune` no longer removes an SWFS instance when run without the Lore service, where the
  instance is not mounted and its path is absent or an empty directory. Its external `.lore` in the global data
  directory shows it still exists, so `lore repository instance list` reports it as live and prune keeps its branch and
  revision anchors
- Fix resolving another user's name failing against an auth service reached by IP address over `http://` or `https://`
- A `lore_string_t` or `lore_binary_t` a caller passes in with a NULL pointer and a non-zero `length` reads as empty
  rather than as undefined behaviour, which is what an empty `lore_array_t` already did. The library answers NULL for
  every empty string it emits, so one can come back in an argument struct beside a length the caller kept itself, and
  the pointer alone decides whether there are bytes to read
- `lore-server`: every local store the configuration asks for is reported at startup, composite tiers included. A store
  the configuration names no path for is reported as unconfigured, wherever the generated fallback points, and a store
  inside a system temporary directory is reported as ephemeral whether it was configured there or generated there; the
  two are independent, since either can hold without the other. A local immutable tier naming a path other than the
  first local tier's is reported as unused instead, every local tier being handed the store the first one creates. The
  ephemeral report previously fired on a path being absent rather than on where the resolved path pointed, so the
  shipped `local.toml`, which sets `/tmp/lore-server` explicitly, drew nothing
- `lore-server`: logs default to `warn` rather than `error`, so a server started without `RUST_LOG` reports the
  conditions an operator has to act on. `RUST_LOG` still wins where it is set, and `RUST_LOG=error` restores the
  previous output
- Fix a moved file being deleted when the change that moves it is realized over a working tree that already holds it,
  which is what re-running an interrupted `lore sync` does. The rename finds nothing at the source it was already
  carried from, and the recovery that follows removed the destination and then skipped rewriting it, because whether the
  content was in place was inferred from the view filter rather than read from the rename. It is now read from the
  rename, so a move whose source is not there is written from the store, and one the rename carried is left alone
- Bound how many subtree tasks the walk between two revisions runs at once, which `lore sync`, `lore status` and
  `lore diff` all drive. It previously ran one task per directory it descends into, all of them live at the same time,
  so peak memory followed the size of the tree rather than the work in flight; a subtree that finds no room in the
  budget is now queued for the task that found it to walk, so neither the tasks a walk holds nor the stack it stands on
  grows with the tree
- Fix a command carried out by the service being answered without part of its output. A relayed call returned its result
  while its events were still being delivered to the caller's callback, so `lore status` could report a repository with
  no staged changes. The events are now delivered before the call returns
- A call relayed to a Lore service that cannot be reached now reports its status and error detail on the `Complete`
  event, as a failing command does, rather than on a legacy `Error` event, so a caller of an `_async` entry point
  receives it
- Fix a relative `--repository` resolving against the Lore service's working directory rather than the caller's for a
  command carried out by the service. `lore repository info`, `lore repository delete`, `lore auth info`,
  `lore auth login` and `lore auth logout` reported no repository or no remote where the path named one. The service now
  resolves the path against the directory the command was run in
- Ignore and view filter files are read as UTF-8, and as UTF-16 of either byte order with a byte-order mark or, where
  the rules are mostly ASCII, without one. UTF-32, truncated UTF-16, and mark-less UTF-16 that cannot be detected are
  refused rather than read as UTF-8 into rules carrying NULs, so UTF-16 whose rules lie outside ASCII requires a mark
- Fix the wrong files being removed when a directory leaves the working tree. Where the directory's node could not be
  read, the removal was decided from the incoming revision's tree and filter rather than the ones the working tree was
  materialized under, so it could name files that were never on disk and leave behind files that were
- A change to a file's executable bit alone is now reported by `lore status --scan`, taken by `lore stage`, and recorded
  on the revision `lore commit` produces. A chmod moves neither the content, the size nor the modification time, which
  were all the comparison read, so the change was invisible from the moment it was made
- `lore sync` and `lore branch merge` keep a locally changed executable bit on a file whose content they carry, rather
  than silently reverting it to the revision's. The bit stands as a local modification and blocks nothing; `--reset` and
  `--force` apply the revision's bit as before
- Fix `lore stage` over a large tree failing with `grab_node_unused returned INVALID on a freshly-allocated block`. A
  freshly allocated block of node slots was published for every other thread to grab from before the thread that
  allocated it took its own, so under enough concurrency the block could be emptied out from under it. It takes its slot
  before it publishes the block
- `lore sync <revision>` advances the branch latest to a revision standing ahead of the one the branch holds, the remote
  answering for it. It was left where it stood, so a sync to the remote's tip drew a `lore status` reporting the branch
  behind the remote and a second sync that did not see it was already there. A named revision moves the latest forward
  only, `--local` and `--dry-run` move it not at all, and a divergent branch keeps the one it has
- Fix a moved file being left behind at the path it came from where the move crosses a mount boundary inside the working
  tree. No rename carries a file between filesystems, and the recovery read a destination that was not there yet as a
  failure of its own, so the file was written at its new path from the store and the old one never removed. A move no
  rename can carry now copies the content and removes the source, and a directory is carried a child at a time
- Fix a branch that replaced a committed folder with a link being invisible to a merge, which silently deleted the link
  and dropped its content. A directory paired with a link is the type change it is: `lore branch diff` reports the
  replacement, and a merge that changed anything below the mount conflicts at the mount
- Changes to configuring the Lore service: The service executable field can be left unset if the service was started
  with lore service run separately. There is no longer a fallback that attempts to use the current binary as the service
  binary.
- `lore-server`: permission checks are refused with `PERMISSION_DENIED` when access is checked against the grpc auth
  service, rather than `INTERNAL`
- Fix `lore push` slowing quadratically with the number of entries in a changed directory. The push and its server-side
  verification pair entries in a single pass over name-sorted lists
- Fix `lore stage` staging a delete for a removed file or directory that was never committed, which `lore status`
  reported as `D` and, after `lore unstage`, as an untracked `A`. `lore sync` no longer refuses to run after
  `lore file dirty` drops such a file
- Fix `lore stage --scan` and `lore status --scan` reading a link mount the working tree does not hold as a deletion of
  the link, which a commit then dropped from the branch. A mount stays absent while its linked repository cannot be
  read, and is kept along with the directories on its path

## v0.10.0 (Sep 17th 2026) [#1170]

### Breaking changes

- Partial hash revision identifiers are refused as `NotSupported`. A revision is named by its whole 64-character hash
  signature, by `[branch]@<number>`, by `[branch]@LATEST` or by `<branch>@<hash>`. Three consequences:
    - The `@` is optional, a target given without it applying to the branch the instance is on: `lore sync 42` is
      `lore sync @42`, `lore sync LATEST` is `lore sync @LATEST`. Digits alone are a revision number at every length but
      64, where they are a signature
    - A revision identifies the branch it was created on, and a branch point can name the child branch instead, which
      `<branch>@<hash>`, `[branch]@<number>` and `[branch]@LATEST` all do. `lore sync`, `lore clone --revision`,
      `lore history`, `lore link update --pin` and `lore link add --disable-branching --pin` follow it, and a sync moves
      onto that branch together with its layers
    - `--search-limit` bounds `lore revision bisect` and layer revision matching alone
- `lore-server`: a `signature` field that is not a whole hash is answered `FAILED_PRECONDITION` on `RevisionInfo`,
  `RevisionTree`, `RevisionDiff` and `RevisionList`. An unset (empty) field is unaffected
- C API: `lore_revision_resolve_event_data_t` replaces its `revision` string with a `target` naming what is resolved (
  `NUMBER`, `LATEST` or `SIGNATURE`) and a `revision` hash carrying the signature for `SIGNATURE`, and reports every
  form resolved on a branch rather than the numbered one alone. Rebuild against the new `lore.h`
- C API: `LoreGlobalArgs.no_atime` is removed; it gated a behavior that was never wired up. Rebuild against the new
  `lore.h`; a caller that zero-initializes the struct needs no other change
- C API: `lore_auth_local_user_info_args_t` renames `with_token` to `with_identity_token` and appends
  `with_access_token`, which emits the repository's authorization (access) token as an `AuthIdentity` event. The
  struct's size and the surviving fields' offsets are unchanged — the new flag occupies tail padding
- `lore-server`: `connection_message_limit` under `[server.quic]` / `[server.quic_internal]` is renamed
  `stream_message_limit` and applies per stream rather than per connection, so with `max_bidi_streams = 8` a value of
  500 allows 4000 requests in flight per connection instead of 500
- `lore-transport`: `user_agent()` moves from `lore_transport::grpc` to the crate root, and `set_user_agent()` is
  replaced by `set_fallback_user_agent_product()`, which supplies the product `LORE_USER_AGENT_PRODUCT` overrides. The
  fallback has to be supplied before anything opens a transport connection, since the product resolves on first read and
  is fixed from then on
- `lore-server`: `[server.auth]` refuses to start without both `jwt_issuer` and `jwt_audience`, and `auth_url` without a
  `[server.auth]` block is refused too; both previously started a server that verified no token at all.
  `lock_service.max_encoding_message_size` moves under `lock_service.general`
- `lore-storage`: Oodle is refused for new fragments as `NotSupported`, and an Oodle environment compression mode is
  remapped to Zstd with a warning. Content already stored under Oodle still reads
- `lore repository create` no longer expands a bare name from `LORE_REMOTE_URL`: a name with no host needs `--offline` /
  `--local`. `lore link add` resolves a bare name against the current repository's own remote
- C API: `LoreSharedStoreListItemEventData` is renamed `LoreSharedStoreListItem`, the `_event_data` suffix being
  reserved for real event types. Rebuild against the new `lore.h`
- C API: a call made after `lore_shutdown` reports the new `ShutDown` code (193) rather than hanging, and a second
  `lore_shutdown` reports an error
- C API: `lore_storage_open_args_t` appends `skip_verify`, taking the struct from 64 to 72 bytes, and
  `lore_storage_get_t` / `lore_storage_get_resolved_t` append `data_out`. Rebuild against the new `lore.h`; a
  zero-initialized struct keeps today's behavior

### Features

- `lore auth info --with-access-token` prints the repository-scoped authorization (access) token for the current user,
  the output-side sibling of the global `--identity-token` / `--access-token` input flags. `auth info --with-token` is
  renamed `--with-identity-token`, with the old spelling kept as an alias
- `lore commit --stats` / `lore push --stats` report files by the action each was staged with, and fragments by what
  became of them — deduplicated, compressed, written to the local store, copied as an association by the peer or
  uploaded — through the `RevisionCommitStats` and `BranchPushStats` events, on the failing path as on the succeeding
  one. `--stats=2` adds the per-fragment `FragmentWrite` stream; `--event-interval <milliseconds>` paces the progress
  events
- `lore-storage`: the local store records each entry's last access, so eviction and compaction rank by least recently
  accessed rather than by write time. A stamp dirties a bucket only when the recorded time moves by an hour or more, so
  a burst of reads costs at most one index write per bucket; the stamp advances regardless, so a smaller move rides to
  disk with whatever writes the bucket next, and dirtying schedules no flush of its own
- `lore-server`: request admission is per QUIC stream rather than per connection, so a saturated stream no longer stalls
  the others. A connection-wide ceiling answers `SlowDown` immediately once requests in handling — those still waiting
  for a stream permit included — reach `connection_inflight_limit`, and a permit wait is bounded by
  `permit_timeout_ms` (default 100ms) rather than by the request deadline. The two refusal paths report separate
  `AdmissionLimit` and `PermitTimeout` metric labels
- `lore`: `lore_storage_get_file_resolved` / `lore_storage_put_file_resolved` are the file-backed pair of
  `lore_storage_get_resolved` / `lore_storage_put_resolved`, streaming a fragment at a time so peak memory follows the
  fragment rather than the content. Publishing costs one round trip at any size, and a tree that did not reach the
  remote whole leaves its key unpublished; `lore_storage_put_resolved` gets that saving too. `offset`/`length` select a
  range as they do for `lore_storage_get_file`
- `lore-server`: forward `RepositoryGet` to a remote Lore server, opt-in via `repository_get` under
  `[server.grpc_public_services.forwarded_requests.enabled_rpcs]`
- QUIC clients announce a user agent through a `ClientIdentify` message — opcode 14 on `lore-storage/0.4`, opcode 20 on
  the replication protocol — sent on connect and on each reconnect and stamped on the `user_agent` field of every
  per-request span. The string comes from `LORE_USER_AGENT`, defaults to the product and library version (
  `lore-transport/<version>` unless `LORE_USER_AGENT_PRODUCT` or a supplied fallback names another product), and is
  normalized through the `user_agent_patterns` allow-list: an agent matching no pattern records `<unknown>` and is
  sampled, a connection that announced nothing records `<no_user_agent>`. An empty, oversized or non-ASCII value is
  discarded rather than rejected
- A child directory holding its own `.lore/` is a nested repository and bounds every walk of the parent's working tree:
  `status`, `diff` and `stage` do not index its contents. Naming one on `lore stage`, or a file inside one, is refused;
  a directory the current revision holds stays tracked
- `lore-server`: every public gRPC service takes an `enabled` flag under `[server.grpc_public_services]`, so one binary
  can serve a restricted subset — a read-only `ThinClientService` deployment among them. `lock_store.mode = "none"`
  builds no lock store, and `lore-server/config/thin.example.toml` is a worked example
- `lore-server`: authorization moves onto OIDC and OAuth 2.0, designed in
  `docs/proposals/2026-08-20-oidc-oauth2-authentication.md`. `[server.auth]` gains `permission_claim`, `resource_claim`,
  `identity_claim`, `resource_id_template`, `resource_wildcard` and `baseline_access`; `jwt_issuer` takes a list, so an
  issuer can change its `iss` without an outage; the JWKS URI comes from OIDC discovery unless
  `[server.auth.jwk].endpoint` overrides it; and `env`, `name`, `preferred_username` and `idp` become optional, so a
  stock provider's token parses. The authorizer is chosen from the configuration, and the defaults match today's
  behavior
- `lore repository create` under `--offline` / `--local` takes no URL, naming the repository after the current
  directory, and a command against a remote-less repository reports `NoRemote` rather than an internal fault
- `lore shared-store list` reports the registry of shared stores Lore now keeps, with the repositories each store backs
- Partial support for SWFS, a virtual filesystem implementation: cloning a repository only, gated behind the `swfs`
  feature, and requiring the not-yet publicly available SWFS drivers
- `lore`: `lore_set_compression_mode` and `lore_set_compression_level` choose how stored payloads are compressed, for a
  caller whose data is already compressed. `LORE_COMPRESSION_LEVEL` still wins, and the first write fixes the level
- `lore`: `skip_verify` on `lore_storage_open_args_t` turns off per-read payload re-hashing on a storage handle, for a
  caller that assures integrity at a higher layer
- `lore`: `data_out` on `lore_storage_get_t` and `lore_storage_get_resolved_t` reads content into a buffer the caller
  already owns, saving an allocation and a copy. The read then emits only `GET_HEADER` and `GET_ITEM_COMPLETE`, ignores
  `streaming`, and fails an item over the stated capacity rather than truncating it
- `lore branch diff` reports the branches and revisions it resolved on the `BranchDiffBegin` event, which the CLI prints
  from, rather than only in a log line
- `lore revision sync` warns when the remote is reachable but its revision could not be read, instead of silently
  syncing against local history: `LoreRevisionSyncTargetEventData` gains `remote_available` and `remote_authorized`
- `loreserver` container images are published from each GitHub release's own binaries to
  `ghcr.io/epicgames/lore/loreserver`, signed with cosign. `X.Y.Z`, `X.Y` and `latest` are `linux/amd64`; the
  `-graviton` tags are `linux/arm64` tuned for Graviton3 and newer. See `DOCKER.md`
- `aarch64-unknown-linux-gnu` builds portable by default rather than always tuning for `neoverse-512tvb`, which SIGILLs
  on older arm64. Opt in with `--config .cargo/neoverse-512tvb.toml --features lore-base/neoverse-512tvb`

### Fixes & Improvements

- `lore sync` and `lore clone` retain the target revision's state and tree records in the local cache, so
  `lore --offline status` works after moving to a revision whose records lack the local-cache-priority flag
- Fix an empty array crossing the C API allocating a block that was never freed, so a long-lived client leaked one per
  event it received — an empty `trace_locations` on the error detail of a successful outcome being the common case. The
  zero-sized allocation was also undefined behaviour
- Fix revisions committed with `lore commit --offline` being unreadable from every other clone once they were pushed,
  where a clone reported a revision's signature and parent but no branch, date or message and
  `lore revision metadata get` answered `Address not found`. Push now walks the second parent of a merge revision,
  oldest first, uploading what each revision owns without offering it as a new latest revision; the walk stops at the
  peer's latest for that branch, or at the branch point for a branch the peer has never seen, and a revision on that
  line that is itself a merge is followed the same way. The server collects a merge against both parents rather than
  only the first, so a push missing the tip of the merged line is refused rather than accepted silently
- `lore revision info` and `lore history` warn when a revision names metadata they cannot read and report it without the
  branch, date and message that blob carries, rather than reporting empty fields with no indication anything was
  missing (`revision info`) or failing outright (`history`). A revision read offline out of a cached revision list has
  nothing to take those fields from, which is ordinary rather than damage
- Fix `lore revision metadata set --binary` leaving its payload on the machine that set it, where every other clone
  resolved the recorded address to `Address not found`. Push now collects the fragments an address-typed value names, as
  it already did for file metadata, and the server verifies them with the revision's other new fragments
- Fix `lore repository instance list` carrying several entries for one root directory. Registering an instance retires
  any earlier registration at the same path. A `.lore/instance` naming another instance is reported `superseded` (
  `stale = 2` in the `RepositoryInstance` event) and retired by any command that reads the list; a path that has gone (
  `stale = 1`) or holds no `.lore/instance` (`stale = 3`, printed `no checkout`) is reported but left for
  `lore repository instance prune`; an instance file that exists but cannot be read leaves the registration alone
  entirely, since only positive evidence makes a registration stale. Stale entries never raise the warning that another
  instance holds the branch, and a lost `.lore/instance` is recovered from the newest registration for the path
- `lore-server`: a forwarded RPC whose peer is unreachable now answers with the origin's own `INTERNAL` status and logs
  the transport failure, instead of passing tonic's `UNAVAILABLE`/`tcp connect error` through. Affects `BranchCreate`,
  `BranchDelete`, `BranchGet`, `BranchList`, `RepositoryCreate` and `RepositoryGet`
- `lore-server`: the channel a forwarded RPC travels over now sends HTTP/2 keep-alive pings while idle and bounds its
  connect and each request, so a silently dropped connection is redialled and a peer that never answers no longer holds
  the forwarding handler. Tuned by `connect_timeout_seconds`, `request_timeout_seconds`, `tcp_keepalive_seconds`,
  `http2_keepalive_interval_seconds` and `http2_keepalive_timeout_seconds` under
  `[server.grpc_public_services.forwarded_requests.client]`, all optional
- `lore`: `lore_storage_get_file` no longer creates, truncates and replaces the destination when `offset` starts past
  the end of the content; the target is left alone and the call reports `INVALID_ARGUMENTS`. A start exactly at the end
  is still a legitimate empty read and writes the empty file
- `lore`: `lore_storage_put_file` and `lore_storage_put_file_resolved` reject a missing or non-regular `path` as
  `INVALID_ARGUMENTS` on the first open rather than after the ten-second transient-failure back-off, so a directory can
  no longer retract a published key by reporting zero size
- `lore-storage`: a write that waits on another task already storing the same address reports the placement the store
  settled at, so content whose fragment tree repeats a leaf — a file of identical blocks — is no longer reported as
  partly absent from the remote, and a key naming it is published instead of withheld
- `lore-storage`: a stopped garbage collection pass gives up within one packfile instead of after a whole compaction
  step, and the stop at the end of every repository command is gone, so background eviction and compaction continue
  across commands
- Fix a directory staged as an add and then removed before any commit being reported as a delete no command could clear;
  a scan now discards the entry with its whole subtree
- Fix `stage --scan` keeping an entry `status --scan` discards, which left the two walks with different trees
- `lore branch merge`, `branch merge into` and `revision cherry-pick` carry nothing from the source revision's metadata
  unless `--inherit-metadata <KEY>` (repeatable) names it, so `created-by`, `reviewed-by` and `change-request` no longer
  follow the source onto the new revision. `message`, `timestamp`, `branch`, `committed-by` and `merged-by` are written
  by the operation that creates the revision, and `cherry-picked-from`, `reverted-from`, `restored-from` and
  `fast-forward-merge` record an operation the new revision is not; neither set is inheritable, including under the `*`
  sentinel
- Fix `lore branch merge` refusing files that carry no local changes; the working file is measured against the node the
  current revision holds rather than the base of the three-way diff, and a file is read and hashed at most once however
  many revisions it is measured against
- `LoreFileHistoryEventData` and `LoreRevisionInfoDeltaEventData` gain a `from_path` field, set for moved files and
  empty for added, modified and deleted actions
- `lore branch diff`, `lore revision diff`, `lore file history` and `lore revision info --delta` print both paths for a
  move, as `V old -> new`
- Fix `lore revision diff` reporting a move as a delete and an add instead of one change
- `lore-storage`: fix local store entries becoming unreachable — reported as `Address not found` while the payload is
  still in the packstore — when the background flush wrote a group's bucket files without writing its level marker,
  leaving the next open to read the group at the pre-fan-out layout of 256 buckets, where nothing a lower level wrote is
  looked up again. Every path that writes a bucket file outside the two-phase commit now commits the group's level
- `lore-storage`: fix `lore branch archive` reporting success while leaving the branch's name-to-id mapping on disk, so
  it kept appearing in `lore branch list`. Concurrent flushes of one group could publish an older snapshot over a later
  write; a per-group flush lock makes the path selection and the writes one unit
- `lore-revision`: the wait for the filesystem clock to pass a recorded modification time is bounded at 10 ms, so a
  coarse or fixed clock cannot block a command on it
- `lore-revision`: fragments the peer reports it already holds are marked durable locally, rather than accumulating as
  non-durable entries eviction cannot reclaim and every later push re-queries
- `lore repository delete` honors `--dry-run`. Name and ID resolution still runs, so a bad URL is still caught
- `lore lock acquire`, `release` and `status` report the server's own denial reason — `resource already locked` among
  them — instead of a generic failure, and a refused batch rolls back the locks it took
- Read verbs no longer connect to the remote where local data is the answer: `lore revision history --branch`,
  `lore file history`, a `branch@LATEST` resolve, and the CLI's user-name lookup. Against an unreachable server a
  `--local` history falls from about 4 s to 150 ms
- `lore stage --dry-run` no longer persists the staged anchor, which made a later real stage report `No changes staged`
  and let a commit pick up the preview state
- `lore-revision`: the Unreal package tag check compares all four bytes rather than three against a four-byte value,
  which made it answer false unconditionally. No file is classified differently today
- Fix merge conflict markers being glued onto the content lines when both sides end the file without a trailing newline,
  which left the file unparsable. `merge resolve mine` and `theirs` restore the committed bytes exactly
- `lore-server`: fix revision-list acceleration sealing the boundary that contains the new revision number rather than
  the one crossed, so a push from 99 to 105 sealed 200 instead of 100 and any lookup from 106 to 200 aborted. Stale step
  keys are discarded by a key rename
- `lore-server`: `RevisionInfo`, `RevisionTree` and `RevisionDiff` resolve a `branch@number` identifier through the
  history step acceleration instead of walking the parent chain from the branch head per request. One implementation
  replaces three, so an unusable step key falls back to a walk everywhere rather than reporting an existing revision as
  missing
- `lore-server`: fix `RevisionList` reporting no newer page whenever the next revision number was not exactly one past
  the page's first item, which a merge or fast-forward makes routine. The next page is found through the branch's own
  step boundaries, and a store failure is reported rather than read as the end of history
- `lore dirty` on a path inside a layer records the marker against that layer rather than the parent. A modified layer
  file was recorded as an untracked add in the parent and a deleted one dropped silently, so neither `status` nor a
  directory-scoped `stage` saw the change
- `lore-base`: a call made after `lore_shutdown` fails rather than hanging forever. The event forwarder was spawned onto
  a runtime already torn down, so the wait for its `End` event never ended
- `lore-revision`: a walk steps the filter one component at a time instead of re-folding the whole path per node — 2.3x
  at depth 12, 8.6x at depth 40 — and a forced walk consults it not at all. Two link crossings were also wrong: `reset`
  carried the link node's own verdict into the content below it, and `unstage` matched a linked file by the linked
  repository's spelling rather than the mount path
- `lore link add` refuses a mount whose source path strictly nests inside another mount of the same repository. Both
  mounts placed the same content under separate pins, so an edit through one went stale in the other and a stage walk
  kept whichever it reached last. Identical and disjoint source paths stay allowed
- `lore-server`: the storage layer's `SlowDown` backpressure reaches the client as `RESOURCE_EXHAUSTED` instead of
  `NOT_FOUND` or `INTERNAL`, and a throttled cache read no longer falls back to a full history walk against an
  overloaded store. A throttled branch head ends a `BranchList` stream rather than omitting the branch
- `lore-server`: a fragment `put` carrying no payload fails hash validation instead of leaving the address unverified
- A `BranchPush` refused for a missing fragment answers `FAILED_PRECONDITION` carrying the address rather than
  `NOT_FOUND`, which the client read as a missing branch and answered by recreating it. The client now reports
  `missing fragment <address>`
- Staging a large changelist no longer logs every path it walks at debug: `Staging path`, `Stage file` and
  `Stage directory` move to trace, and a resolved case is reported at debug only where it had to be corrected.
  `Stage file` also named a path that did not exist
- `lore-server`: an HTTP/2 POST whose body carries no decodable gRPC message — what a plain `curl` sends — is reported
  as `InvalidArgument` rather than `Internal`, so `Internal` keeps meaning a genuine server fault
- Performance: staging a path at depth *n* stats the working tree once rather than once per component; a scan allocates
  5.9 times per file on an unchanged tree where it allocated 16.2; and a one-item storage batch runs on the calling task
  rather than spawning
- `lore-server`: a trusted internal QUIC connection's `ClientIdentify` user agent is recorded as sent, so a deployment
  need not list its own server-to-server agents in the user-agent filter
- A staging walk settles a type replacement rather than only reporting the delete and the add: the displaced subtree
  takes a staged delete and a directory replacing a file is descended. Staging twice is idempotent, a staged delete is
  taken back where the file is still there, and a staged add records the size and mode measured. `branch merge`,
  `cherry-pick`, `revert`, `reset` and `unstage` hold one filesystem operation per diff instead of freezing the working
  tree per file
- Link and layer paths are reported and resolved from the working tree rather than from the linked repository's own
  spelling. A layer mounted away from its source path can be synced and reports changes at the mount, a conflict from a
  subtree link is named under the mount rather than dropped, `status --check-dirty` clears and counts markers inside
  layers, and staging a delete below a shared pre-created ancestor resolves the link chain from there
- `lore unstage` and `lore reset` route a path inside a layer into the layer's own states, where before both reported
  success and left the layer untouched — leaving a staged layer blocking `lore sync` with no route forward that kept the
  file. `reset --revision` on a layer path is now refused
- `lore-io`: `create_dir_all` succeeds wherever the directory is already there, so a Windows container bind-mount root —
  which answers `PermissionDenied` for `mkdir` — no longer fails `sync` and `clone`. Rust 1.94 narrowed the standard
  library to forgiving `AlreadyExists` alone
- `lore-storage`: healing a failed `verify_fragment` drops the associations naming the bad payload rather than clearing
  the payload pointer. A payload-less entry answered a full match while serving nothing, so peers were told content was
  available that the store could not serve, blocking re-upload. Obliterated entries are left alone
- `lore-server`: a replicated store's `get_metadata` and `query` take the two reserved priority QUIC streams instead of
  queueing behind bulk fragment traffic — p50 falls from 15.4 ms to 1.6 ms behind 128 concurrent 32 KiB gets
- Switching between branches that mount one repository at different source paths no longer coalesces the mount delete
  and add into a rename, which left the mount holding a subtree its own source path does not name. A link node's
  identity names the repository it mounts, which every mount shares, so it is no longer read as a file identity
- `status --scan` reports each change as the walk finds it rather than after the whole diff is collected, so the first
  event fires during the scan and the walk always completes, leaving dirty marks whole
- `lore`: a storage session error keeps its classification instead of being wrapped as internal, so a peer's `SlowDown`
  reaches the retry paths in read and write again
- Documentation that described `Repository.created`, `Branch.created`, the thin-client commit timestamp and
  `lore revision history --date` as Unix epoch seconds is corrected to milliseconds. No wire or on-disk value changes;
  `--date` was the one place the documented unit made the filter never match

## v0.9.0 (Aug 28th 2026) [#782]

### Breaking changes

- `lore-credential`: auth tokens move to `tokenstore.toml` under a `tokenstore_encryption_key` key, with nothing
  migrated from `tokens.toml`; old and new clients keep entirely separate credentials, so expect one `lore login` per
  generation. An unmigrated store reads as `Not authenticated` and `--remote` reads answer empty rather than failing, so
  scripts gating on `lore status` should check the exit code
- `lore-server` (AWS store): the fragment describing a payload now travels on the S3 object as an
  `x-amz-meta-lore-fragment` header instead of in a DynamoDB record, and the fragment metadata table is replaced by a
  fragment state table holding lifecycle state alone (a row's presence means the hash exists). Existing objects are not
  rewritten — they are read through a fallback to the old table, which must stay configured for them to remain readable.
  Under `[plugins.aws.immutable_store]`:
    - Existing deployment: set `dynamodb_fragment_state_table` (required, no alias — start fails without it; normally
      the table that held fragment metadata, since the key schema is unchanged and the two row shapes coexist) and
      `dynamodb_fragment_metadata_table` (enables the fallback read, normally the same value; accepts the older
      `dynamodb_metadata_table` spelling). Roll out as a full stop followed by a full start — old and new servers must
      never run at the same time. To move the data across and retire the old table, see
      `contrib/aws-migrate-0.9.0/README.md`
    - New deployment: set `dynamodb_fragment_state_table` only. Leaving `dynamodb_fragment_metadata_table` unset
      declares that no object predating the change exists, so no fallback read is ever issued and an object without its
      own metadata is reported as damaged
- `lore.model.v1` and `lore.thin_client.v1`: `Repository.created`, `Branch.created` and `Revision.timestamp` now carry
  Unix epoch milliseconds instead of seconds
- C API: `LoreMetadataType` discriminants are now stable integers shared between `lore.h` and the on-disk metadata
  buffer, and `lore_revision_tree_metadata_set` takes typed `(key, LoreMetadata)` batches instead of text plus a format
  tag; callers using the enum names need a recompile, and callers hard-coding the old numeric values must update them
- `lore-server` (replication protocol): `ExistsBatch` is replaced by a batch `Query`, and `Get` / `GetMetadata`
  responses carry the full `StoreGetData` including the partition. Retired opcodes are reserved rather than reassigned,
  so a peer on the previous protocol is rejected instead of misreading a response — roll replicas and their upstream
  together
- C API: failures that previously reported `-1` (internal) now carry the specific code where one applies, because an
  error crossing an internal boundary keeps its variant instead of collapsing. `lore_revision_tree_metadata_set` and its
  file equivalent can now return `SlowDown` (5), `NotAuthorized` (7), `Maintenance` (11), `NotAuthenticated` (12),
  `NoRemote` (14), `NotConnected` (17) and `NotSupported` (18); remote store reads and writes add `Disconnected` (6);
  and connecting with no stored credential reports `NotAuthenticated` rather than an internal fault. A caller treating
  every non-zero status alike is unaffected; one that branches on `-1`, or reads `-1` as `retrying will not help`, now
  sees retryable and re-authenticable codes on paths that previously only ever produced `-1`, and should handle them
  before upgrading

### Features

- `lore-io`: new runtime-independent asynchronous file I/O engine backing Lore's file access — positional owned-buffer
  operations on a bounded, idle-reaped syscall pool, upgraded automatically to `io_uring` on Linux and overlapped I/O on
  Windows, with vectored scatter/gather reads and writes. `LORE_IO_BACKEND` overrides the choice; internals in
  `docs/developing/internals/file-io-engine.md`
- `lore`: `lore_revision_tree_commit` freezes a handle's in-memory tree into a revision and advances the branch tip by
  compare-and-swap, taking the branch from the handle's `branch` metadata key. Commit is exclusive and all-or-nothing —
  a failure leaves the handle where it was with every edit still staged — so services can publish revisions with no
  working tree on disk
- `lore`: `lore_revision_tree_delete`, `_modify` and `_move` join `add` as batch verbs, each validating the whole batch
  before any node changes and emitting a `*_COMPLETE` per entry plus one `BATCH_COMPLETE`. A node from the loaded
  revision is staged for deletion and reversible, one added through the handle is discarded outright, and
  `staged_action` on the child and node-info events reports what is pending. Moving into a linked repository is not
  supported yet
- `lore`: batched `lore_revision_tree_metadata_set` / `_get` / `_clear`, with `LoreMetadata`, `LoreMetadataType` and
  `LoreBinary` reworked into owning, self-describing types so binary metadata can cross event callbacks and both wire
  formats (see Breaking changes)
- `lore`: a revision tree handle holds its own store reference, so closing the parent storage handle leaves it usable;
  orphaned handles are closed when an IPC connection drops, and `lore::shutdown` drains tree handles first
- `lore`: `lore_storage_get` / `lore_storage_get_file` take `offset` and `length` per item to read only a slice of the
  content, pruning the fragment tree so the work is proportional to the range rather than the content size (a zeroed
  pair still reads the whole content; a start past the end is rejected with `INVALID_ARGUMENTS`)
- `lore`: `lore_storage_get_resolved` / `lore_storage_put_resolved`, also over QUIC and gRPC, resolve a key belonging to
  another system — an asset id, a build id — and act on the content it names in one request instead of two. A read
  resolves local-first and caches the mapping it learns, verifying the root fragment against the resolved hash; a write
  stores content before publishing the key and is last-writer-wins. Design:
  `docs/proposals/2026-08-02-resolved-storage-operations.md`
- `lore branch archive --include-layers` / `--layer <path>` archives the branch in every configured layer, or in the
  layer at one mount path; the default still touches only the repository it ran in, since archiving deletes and a layer
  owns its own branch lifecycle
- `lore --identity-token <token>` / `--access-token <token>` (and the matching `LoreGlobalArgs` fields) use
  caller-supplied tokens instead of the credential store, for CI runs and stateless services. Tokens are ephemeral and
  never stored, the identity is read from the token, and both conflict with `--identity`; given only an access token, an
  operation needing an identity token fails rather than falling back to a stored one
- `LoreRepositoryCreateArgs` / `LoreRepositoryCloneArgs`: `use_shared_store` is replaced by a `LoreSharedStoreMode`
  enum (`Inherit` = 0, `Enabled`, `Disabled`), so a caller can explicitly refuse shared-store backing on a machine where
  `use_shared_store_automatically` is set (a zero-initialized struct still follows the machine setting)
- `ImmutableStore`: `exist` / `exist_batch` are unified into a batch `query` answering with the best match level found,
  and `get` / `get_metadata` drop their match-level parameter for one `StoreGetData` carrying the fragment, the level
  and an optional payload. The contract — never over-report, obliterated never matches, reads agree with each other and
  name where a match was found — is written into the trait and enforced by a conformance battery every store runs
- `lore-server`: fragments arriving Oodle-compressed on `put` are transcoded to Zstd as the first step of retiring
  Oodle, gated behind the `oodle` feature and opt-out at runtime with `LORE_DISABLE_CONVERT_OODLE_ON_PUT`; the address
  is unchanged, since identity is over uncompressed content
- `lore-aws`: `MetadataMigrator` and `run_migrator` drain legacy DynamoDB fragment-metadata rows into the new
  S3-object-metadata plus state-row model, by segmented parallel scan across a pool of consumers. An accurate non-Oodle
  codec is re-uploaded as-is to set its S3 headers, Oodle and mismatched codecs recompress to Zstd, and a run is
  idempotent and resumable. Packaged as a standalone tool under `contrib/aws-migrate-0.9.0`
- `lore link add` / `remove` / `update` / `reset` / `list` now work on a nested link — one mounted inside another link's
  subtree — mutating the innermost repository's registry and propagating outward, and `lore commit --link <nested path>`
  commits each intermediate link as a real revision before repinning. Nested-link merge is not covered yet
- `lore-server`: the legacy `urc.rpc.RevisionService` diff and tree gain `link_partition` and `tracking` fields, so a
  consumer can tell which repository a changed path resolves under and whether a link follows its parent branch or is
  pinned (both additive on messages already marked deprecated)
- `lore-server`: `presigned_url_extra_content_types` and `presigned_url_denied_content_types` under `[server.http]`
  adjust which `Content-Type` values a redeemed presigned URL may carry. They extend rather than replace the built-in
  safe set, a type in both lists is denied, and browser-executable types can never be added — the server refuses to
  start instead of failing open
- `lore-proto`: thin-client `TreePath` exposes file size and mode, so a `ThinClientService.RevisionTree` caller gets
  both without a second request
- `lore`: self-signed certificates installed in the OS trust store are now honored (`reqwest`'s
  `rustls-tls-native-roots`), which local development against a self-signed server needs
- `lore link info <path>` reports one link's mount and source paths, its branch and whether that branch is followed or
  pinned, the pinned and remote-latest revisions, the link flags, and the staged state and staged file count inside it;
  the remote revision is omitted when offline

### Fixes & Improvements

- Thread model: the network transport moves onto its own runtime, the rayon compute pool is gone with compression,
  hashing and chunking now inline on the core workers, and the blocking pool shrinks to two threads. `LORE_MAX_THREAD`
  is now an absolute cap. Clone throughput 1.4 → 2.5 Gbps, large-commit peak memory 6 → 4 GiB
- `lore-storage`, `lore-revision`: the store buckets, packstore, fragment chunker and remaining `tokio::fs` / `std::fs`
  calls all run through `lore-io`, so a common-case bucket load completes in one dispatch and a flush gathers header,
  index and entries in one vectored write; filesystem locks back off asynchronously instead of parking a thread
- `lore-storage`: fix a file shorter than its measured size being taken as an early end, producing a chunk list covering
  fewer bytes than its root fragment recorded; the read now fails
- Windows: no handle the I/O driver opens permits a second writer, while a read-only open still shares reads and
  deletion so replace-by-rename works. `ERROR_SHARING_VIOLATION` is now transient and retried, so materializing a file
  being hashed waits rather than fails
- Fix `lore branch merge` under a sparse view dropping changes outside the view, leaving the branch divergent from the
  one it recorded as merged; nodes now merge regardless of the view, which gates only on-disk work, and an out-of-view
  conflict adopts theirs as `StagedMergeTheirs` (adopting untouched excluded subtrees whole made it 34% faster on
  110,000 files)
- Fix a link added, removed or repinned on one branch not surviving the merge that brought it over, where `link list`
  reported nothing and `lore stage` failed with `Link not found`; the registry now follows any link node a change set
  stages or deletes, and a row both sides moved refuses the merge
- Fix a revision moving a link's pin producing an empty diff when the subtree was byte-identical, and entries from a
  linked repository naming no repository so content could not be fetched from the right partition; a new `link_read`
  policy gives a caller authorized only for the parent one change for the mount path
- Fix `lore branch create` failing with `Branch <name> already exists` when a linked repository held the requested
  branch id under another name; the cascade adopts it and reports a `LinkBranchCreate` event with a `reused` flag
- Fix concurrent link edits overwriting one another: `link add`, `update` and `remove` released the runtime lock between
  reading and storing the registry, and now hold one write lock across it
- Fix `lore push` on a repository with links reporting the parent's revision against a branch that exists only in a
  linked repository; push events now carry the repository and branch they report (repository state was always correct)
- Fix `lore stage . --scan` pinning a staged revision for unchanged layers, which aborted the next `commit` with
  `Nothing staged` and made `branch switch` refuse with `Layer has uncommitted staged changes`; a layer is pinned only
  when the walk left its state dirty
- Fix `lore sync` leaving a layer's staged state on the revision it moved away from, so the next commit refused it as a
  stale parent or under `--force` reverted what the sync brought in; sync now refuses when a layer holds staged work and
  otherwise rebases onto the new pin
- Fix `lore layer remove` discarding a layer's staged work silently and leaving staged adds on disk; removal now counts
  staged files and refuses without `--force`
- Fix an unreadable `.lore/config.toml` or `layer.toml` presenting as a repository with no remote or no layers, which
  the next save made permanent; both now default only for `NotFound` and save through a renamed `.tmp` sibling
- Content the peer already holds is no longer re-uploaded: where a query reports the hash under another context or
  partition, the client asks the peer to duplicate the association instead of sending the payload, on `lore push` and
  direct writes alike
- `lore-revision`: the composite store caches metadata resolved by `query`, not only `get_metadata` hits, so a `query`
  -heavy workload stops hopping to the durable store; capped by `cache_metadata_semaphore_size` and limited to
  durable-sourced results, and `should_cache_query_results` is renamed `cache_metadata`
- `lore-server`: `get_metadata` now fans the durable store out in parallel with read replicas and sends a dedicated QUIC
  command, instead of falling back to a local `query` that reports only the durably-stored flag; edge replicas answered
  incorrectly and always paid a cross-region trip
- `lore-server`: the JWK service picks up key rotation without a restart; a verification failure only a key could
  explain triggers one refresh, so material replaced behind an unchanged `kid` no longer rejects every token
- `lore-server`: JWK fetches are bounded by timeouts, a pooled client, single-flight collapsing of concurrent misses and
  a minimum interval, with an empty cache never throttled so start-up always proceeds. The document is capped at 1 MiB
- `lore-server`: a JWK set is validated key by key — an unusable key is skipped rather than failing the fetch, a key
  whose `alg` family does not match its `kty` is refused, and an empty result errors. RS256 is inferred for an RSA key
  omitting `alg`, which previously failed start-up against Microsoft Entra ID
- `lore-server`: why a JWT verification failed no longer reaches the caller — `bad signature`, `expired` and
  `no such key id` are an oracle for someone unauthenticated; the reason goes to a debug log
- `lore-server`: fix a stored-XSS vector on presigned-URL redeems, where bytes could be served from the Lore origin
  under a caller-chosen `Content-Type` such as `text/html`; a deny-by-default allowlist rejects at mint, coerces on
  redeem, and every response carries `nosniff` and a `default-src 'none'; sandbox` CSP
- `lore-server`: `binary/octet-stream` is allowed in the presign allowlist and served verbatim — S3 assigns it to
  objects uploaded without an explicit `Content-Type`, so callers forwarding S3 metadata had theirs coerced
- `lore-server`: `RepositoryMetadataGet` and `RepositoryMetadataSet` now perform the real-time authorization check the
  other repository handlers do, so a client cannot read or write metadata on a repository it has no access to
- `lore-transport`: fix a per-item failure on the streaming storage RPCs being reported as a stream trailer, which
  killed the HTTP/2 stream and every request multiplexed onto it; outcomes now travel in-band, and field numbers are
  preserved so get responses stay wire-compatible
- `lore-transport`: fix gRPC reconnect not re-reading the epoch per attempt, and storage streaming having no working
  reconnect at all; a dead stream now rotates once across concurrent callers and replays outstanding requests
- Fix `lore` commands taking about 30 seconds when a hostname resolved first to an address family the server was not
  listening on, commonly `localhost` on `::1`; the QUIC client now follows Happy Eyeballs (RFC 8305), 30.05 s to 0.38 s
  on the reproduction
- `lore-transport`: fix the per-stream in-flight counter never being decremented on error or cancellation, which
  degraded priority routing and left the selector round-robining; also fix `add_stream` storing a count one short of the
  streams it opened
- `lore-aws`: `query` is backward compatible with fragments described by the legacy metadata table, so `BranchPush` no
  longer reports content as missing when it is stored but described by the old row
- `lore-aws`: the mutable store's zero-expected compare-and-swap matches the local store now — a row holding a zero
  value is treated as absent — so an empty branch pointer cannot make a swap report success without the write landing
- `lore-storage`: a corrupt mutable-store bucket is no longer reset to empty on an authoritative store, where it
  silently dropped branch heads, metadata and instance registrations with no upstream to refill from;
  `authoritative: true` makes it a hard error
- `lore-credential`: fix the credential store rewriting its keyring entry on every stored token, which on macOS prompted
  for keychain access from every application linking Lore and again after each rebuild; each seal now draws a random
  nonce instead of persisting a counter
- `lore-credential`: fix any keyring error being read as `no key`, so one denied prompt regenerated the key and wiped
  the token store for every Lore process on the machine; only `NoEntry` counts as absent
- Fix a notification subscription that had stopped on its own still counting as live, so every later subscribe returned
  success without subscribing; liveness is now checked by cancellation token and task state
- Fix `lore history --branch <name>` returning an empty listing for a branch that exists only on the remote; the local
  branch is preferred and the remote head used when there is no local history
- Fix an infinite loop walking revision history for a deleted file, where a zero parent hash was handed to
  `State::deserialize` repeatedly; a three-way diff base no longer resolves to the zero revision
- `lore-revision`: the diff and merge base search no longer errors as `Divergent` when it cannot prove divergence —
  branch points sharing no revision fall back to the older one, and two points with the same revision number
  short-circuit instead of spending a full search budget
- Fix `lore reset <path> --revision <revision>` failing when the path is currently a directory but was a file in that
  revision
- Fix `lore sync` undoing a change from a file to a directory; where an add and a delete carry the same node name, the
  delete now takes the `from` node
- Fix a clone failing with `Failed to create directory` when the parent already existed but was not created by that
  call — a bind-mounted clone root or a drive root
- Fix `lore stage --targets <file>` hanging on a large list, where `dedup_to_supersets` rescanned every kept path per
  candidate and timed out around 900,000 paths; sorting in subtree order takes the collapse from quadratic to O(n log n)
- `lore stage` no longer grows memory in proportion to the work ahead of it; in-flight tasks are capped at 1000 and
  drained as they complete, and directory fan-out is bounded by a semaphore, processing a child inline when no permit is
  free
- `lore-revision`: node allocation during staging no longer holds a single-permit mutex across its whole scan, and
  `node_add` skips the 65 KB zero allocation it issued when clearing a recycled slot whose metadata block was never
  written
- `lore-revision`: the per-node stage log lines drop from debug to trace; they fired once per file and made `--debug`
  unusable during a large stage
- `lore-base`: fix `Hash`, `Context`, `Partition` and `Address` being unreadable under non-self-describing formats such
  as `bitcode`, which made any record carrying one fail to decode; a truncated address is also refused rather than
  becoming the zero address
- `lore-server`: pushing a revision hash that does not exist in the immutable store returns `NotFound` instead of a
  generic `Internal` status
- `lore-server`: `LORE_SERVER_MAINTENANCE=1` stops the internal QUIC server serving its port while the internal gRPC
  server keeps a stub listening, so health checks get `unavailable` rather than a closed port
- `lore-telemetry`: `size_histogram` gains explicit power-of-two boundaries from 64 B to the 256 KiB
  `FRAGMENT_SIZE_THRESHOLD`, so `put` and `get` distributions are no longer cut off at the OpenTelemetry defaults
- `lore-error-set`: new `chain_err_from` chains a discrete error onto an error-set enum without destructuring a
  `Traced<E>`; call sites that discarded the originating trace now preserve it
- `lore-revision`: `LoreRevisionDiffFileEventData` gains `from_path`, so a receiver of `LORE_EVENT_REVISION_DIFF_FILE`
  can reconstruct where a moved or copied file came from
- `lore`: the batch verbs' id fields are named apart — `entry_id` per entry, `batch_id` on the batch args and
  `BatchComplete` — and `parent_entry` becomes `parent_entry_index`
- SWFS groundwork: the stale commented-out integration is replaced by a real interface whose types are always available
  while its methods compile only under the `swfs` feature; `InstanceOperationImpl` lets an operation in a linked or
  layered repository be finalized alongside the main one
- New `iteration` Cargo profile inherits `release` with LTO off, cutting link time when rebuilding frequently
- `lore-revision`: reject directory traversal and dot-prefixed segments in repository path components
- `lore-aws`: guard against maliciously large S3 payloads
- `lore-storage`: fix data loss in the lazy fan-out redistribute path, where buckets left unmarked after a fan-out could
  be overwritten from the stale on-disk layout by a reader racing the flush
- `lore-storage`: decide the legacy bucket layout per group rather than per store, so one written group no longer pins
  every other group at the maximum bucket count on the next open
- `lore-storage`: file modification detection no longer downloads content — `file_matches` transfers the stored header
  and, when fragmented, its fragment lists, then compares chunks against the file's own bytes
- `lore-storage`: fix a dropped consumer on a streaming read being reported as an error instead of draining gracefully
- `lore-storage`: build zstd compression and decompression contexts in workspaces this crate owns, with the pool holding
  at most 32 and further concurrency building one per call
- `lore-storage`: remove `allow_partial_fragment` from the local store, so a `lore-server` local store can cache
  `get_metadata` results
- `lore-storage`: obliterate a fragment tree without holding a lock across it, and drop an unreachable sink path from
  the defragment pipeline dispatch
- `lore-storage`: key file modification times by the lowercase path string, unifying them with node-name matching
- `lore-server`: `ReplicatedImmutableStore` implements the `Copy` message and returns the `Context` a query matched
  under, instead of leaving clients to assume the default
- `lore-transport`: a persistent connection updates the authn and authz tokens it presents, and authz exchange results
  for caller-supplied identity tokens are no longer written to the token store
- `lore-auth`: a missing token reports `NotAuthenticated` instead of a generic internal error
- Fix clone not applying view filtering inside linked and layered repositories, where `clone_node` was called with a
  path relative to the linked repository rather than the mount point
- `lore-revision`: refuse a `link remove` that would destroy local edits
- `lore-revision`: fix non-ASCII lowercase handling when building relative paths
- Fix the originating trace being lost through `::internal` and `::internal_with_context`
- `lore-revision`: staging does less work per path — targets resolve concurrently, each walks from its pre-created
  ancestor rather than the root, a directory's children are read once and matched by binary search, a shared directory's
  case resolves once, and paths are neither re-stat'd nor rebuilt through extra string allocations
- `lore-revision`, `lore-storage`, `lore-base`: allocation and locking trimmed on the hot paths — the dirty tree walks
  from an explicit stack and names into one buffer, node blocks serialize from the lock without a copy and deserialize
  once rather than once per waiter, merkle tree blocks come from their own heap, short ASCII names fold to lowercase on
  the stack, the log level reads from an atomic, and successful operations no longer allocate error strings
- `lore-transport`, `lore-revision`: reuse the lazy `StorageSession` wrapper across fragment writes instead of
  rebuilding it per write
- `lore-io`, `lore-revision`: verify a path's case by asking the filesystem for the name instead of listing its
  directory
- `lore-aws`: box SDK error payloads to shrink the `lore-aws` error types

## v0.8.6 (Jul 29th 2026)

### Breaking changes

- `lore-client`: logging CLI arguments are now strictly mutually exclusive; a command line passing conflicting logging
  flags is rejected instead of silently accepted
- Auth `login`/`info` now return `NotSupported` (code 18) when the server has no auth endpoint configured, and surface
  `NotAuthenticated` / `Disconnected` instead of a generic internal error (code -1); scripts branching on these exit/FFI
  codes must be updated
- Presigned URL vending (`POST /v1/repository/{id}/content/{address}/presign`) is now restricted to service accounts;
  normal user tokens can no longer mint presigned URLs

### Features

- `lore-server`: forward `BranchList` and `RepositoryCreate` to a remote Lore server when configured (extending the
  existing `BranchCreate`/`BranchDelete`/`BranchGet` forwarding)
- `lore`: add a batch node-add verb (`lore_revision_tree_add`) to the low-level revision API, landing a whole subtree
  atomically
- `lore`: run the service process (`lore service run`) on Linux and macOS, not just Windows
- `lore`: validate that all strings passed to the C API are valid UTF-8, rejecting invalid input up front with a named
  field
- `lore-server`: support file:// JWKS endpoints in the JWK service
- `lore-revision`: optional `durable_delay` on the composite store so read replicas can answer before the durable tier
  is queried
- `lore-storage`: self-heal corrupt (torn-write / zero-filled) mutable store buckets instead of failing to open the
  store
- `lore-server`: add `Cache-Control` headers to the presigned-URL redeem endpoint for immutable content
- Add a self-contained Terraform example for an AWS primary + edge deployment under `contrib/aws/`

### Fixes & Improvements

- `lore-storage`: `read_into` now respects the requested byte range for single-fragment reads (partial reads of files ≤
  256 KiB no longer fail)
- `lore-server`: fix JWK refresh cache check so a missing/rotated key triggers a fetch, allowing key rotation without a
  restart
- `lore-credential`: require a DNS label boundary when matching a dotless JWT `aud`, and allow exact apex-domain
  matches, closing an audience-suffix leak
- `lore-revision`: preserve dirty move status through `status --scan` instead of degrading it to delete + add
- `lore-revision`: bound commit read memory with a travelling fragment permit, and cap directory recursion fan-out
  during commit
- `lore-revision`: drain staging tasks on all error paths so failures propagate cleanly
- `lore-storage`: batch FastCDC chunking to cut per-chunk overhead on medium/large writes
- Add transport connect timeouts so local reads no longer stall on an unreachable remote
- `lore`: use the calling process's working directory for absolute-path resolution in the service process
- Fix `lore_branch_switch` and `lore_branch_reset` to restore the branch name→id mapping (so the branch appears in the
  local list), with a `--force` override
- `lore-revision`: fix `restore` failing with `Dirty node remain after nodes were committed`
- `lore-base`: update vendored `rpmalloc` to 2.0.1

## v0.8.5 (Jul 15th 2026)

### Features

- Implement the low-level revision-tree read verbs on the C API: `tree_load`/`tree_close`, `resolve_path`,
  `list_children`, revision & node `info`, and `node_path`
- Forward `BranchCreate`, `BranchDelete`, and `BranchGet` to a remote Lore server, opt-in per-RPC under
  `[server.grpc_public_services.forwarded_requests]`
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
