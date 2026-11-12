# Release Notes

Release notes for the open source Lore project. Releases before v0.8.4 predate this file; see the
[GitHub releases](https://github.com/EpicGames/lore/releases) page for the published record.

## v0.14.0 (Nov 12th 2026) [#2388]

### Breaking changes

- `lore-revision`: `lore branch metadata set` and `lore branch metadata clear` (`lore_branch_metadata_set`, `lore_branch_metadata_clear`) refuse the `archived`, `restored`, `restoring` and `released` keys, which archives and restores now own, and Lore Server's `BranchMetadataSet` refuses a swap that adds, changes or removes them. A value an application stored under one of these names before changes what archives and restores of the branch do. Remove or rename it before upgrading: after the upgrade, neither command nor `BranchMetadataSet` can remove it.
- `lore-credential`: a command relayed to the Lore service without a `LORE_AUTH_STORE` uses the secure store, not the store the service's own environment names. A program served by a service started with `LORE_AUTH_STORE=fallback` sets `LORE_AUTH_STORE=fallback` itself, or, for the `lore` command, `[auth] store`.
- `lore-credential`: on Unix, a link in place of a token store file is refused rather than followed. Replace the link with the file.
- `lore-client`: a `LORE_AUTH_STORE` naming neither `secure` nor `fallback`, which read as the secure store, stops every `lore` command, as does a user-level `config.toml` that can't be read, isn't TOML, or holds an `[auth]` table that doesn't parse. Correct the value or the file.
- `lore`: a call relayed to the Lore service carries one more variable, so a client and a running service from either side of this release misread each other's calls. End a running Lore service's process after upgrading.
- `lore-revision`: `lore branch create` and `lore_branch_create` given a name that isn't valid, or run on a personal branch, which can't have children, fail with `InvalidArguments` (3), where they failed as an internal error (-1, exit status 255). Given a zero id, they fail with `InvalidArguments` as well, where they created a branch with id zero. Scripts and C API callers that test a create given an invalid name, or run on a personal branch, for 255 or -1 should test for 3 as well, and one that creates a branch with a zero id should pass another id or none.
- `lore-revision`: `lore clone --root-file` with a root file that does not exist exits with code 82 (file not found), as `lore sync --root-file` does, rather than 83 (node not found). `lore_repository_clone` returns 82 for it in its return value and in `Complete.status`. Check for 82 where a caller checked for 83.
- `lore-capi`: `lore_repository_instance_delete_begin_event_data_t` no longer has a `dry_run` field, and the JSON form of the `RepositoryInstanceDeleteBegin` event no longer has `dryRun`. An event doesn't repeat the arguments of the call it belongs to: a caller that needs to know whether a delete is a dry run reads the `dry_run` it set in `lore_global_args_t`. Rebuild against the new `lore.h`. A client and the Lore service it relays to must come from the same build, or a relayed `instance delete` fails to decode the begin event; restart the service after upgrading.
- `lore-server`: Make a mutable store plugin's compare-and-swap to zero remove the key, its swap from zero to zero write nothing, and its list leave out a key holding zero, as the local and DynamoDB stores do. A plugin that keeps a key holding zero keeps one for each branch name a branch or repository delete releases, and Lore Server's branch list warns about each of them on every listing.
- `lore-server`: Make a mutable store plugin's compare-and-swap refuse a swap from a non-zero value on an absent key, answering zero, and answer a value the key held while the call ran. A plugin that makes such a swap, or answers a value read before the call, reports a name released or branch metadata swapped that the key didn't hold.
- `lore-revision`: `lore branch switch` and `lore branch reset` (`lore_branch_switch`, `lore_branch_reset`) onto an archived branch, given its id, now leave the branch archived. They used to map its name to it again, and with `--force` took the name from another branch that held it. Restore the branch with `lore branch create <name> --id <id>`, before or after the switch or reset. A switch or reset given the id of a branch the remote holds live still maps its name in the local store, as a lookup by its name does.
- `lore-server`: A push to an archived branch, through `lore.revision.v1` `BranchPush` or the deprecated `urc.rpc` `BranchPush`, now fails with `NOT_FOUND` and leaves the branch archived, where it mapped the branch's name to it again. `lore push` answers it as it answers a branch archived during the push: it restores the branch with a create, given its id, and pushes again, so pushes from the `lore` client still restore the branch. Another client that pushes to an archived branch restores it with a branch create first. A push to an archived branch whose name maps to another branch with metadata still fails with `ALREADY_EXISTS`, as the create that restores the branch would, which now includes a holder whose metadata names it otherwise, in another case or not, and one whose holder's metadata key can't be read fails with `INTERNAL`, where Lore Server took the name from either.
- `lore-revision`: the push events `lore_branch_push_fragment_begin_event_data_t`, `..._fragment_progress_...`, `..._fragment_end_...` and `lore_branch_push_branch_create_end_event_data_t`, and the revision update and revision push update events, start with the `repository` and `branch` they belong to. Branches push at once, so the events of different branches interleave, in no fixed order between branches. Rebuild against the new `lore.h`, and track push progress per repository and branch.
- `lore-client`: `lore push` shows the fragments of every branch it pushes at once in one progress bar.
- `lore-revision`: a push of a revision whose links can't be read fails with the read's error, such as `AddressNotFound`, rather than pushing the revision without its links.
- `lore-server`: the messages a QUIC storage request logs when it fails or times out now carry the `lore_server::quic` target, where they carried `lore_server::quic::stream_handler`. A log filter that names `lore_server::quic::stream_handler` no longer selects them; name `lore_server::quic` instead. The container sockets log their failed requests under the same target.
- `lore-revision`: `lore repository instance move` and `lore_repository_instance_move` refuse a target that already exists, is inside the instance (through a symbolic link too), is below a file, has no existing directory above it, or is already an instance mount with `InvalidArguments` (3), as a dry run does. A target that already exists or is already an instance mount was refused as an internal error (-1, exit status 255). Scripts and C API callers that test a refused move for 255 or -1 should test for 3. A symbolic link at the target that leads nowhere counts as existing, where the move replaced it
- `lore-storage`: These storage requests fail with `NotAuthenticated` (16) or `NotAuthorized` (17) when the server refuses them, where they failed with `NotConnected` (29): a fragment read, a key resolve, a key a resolved write publishes or retracts in a request of its own, a C API storage metadata read or obliterate, and a C API storage copy the server refuses as unauthenticated or whose fallback upload it refuses. The error keeps the server's reason, where it gives one, on its trace. Scripts and C API callers that test a failed storage request for 29 should test for 16 and 17 as well.
- `lore-storage`: A fragment read or key resolve the server refuses is retried once, at once, on a fresh session, since an expired token is refused and a fresh session can carry a fresh one. It was retried five times, with back-off between attempts.
- `lore-storage`: `lore file audit` reports a line as link-unavailable when the server refuses to read the content of its earlier version in a linked repository, and the instance holds that linked revision's state. Such an audit failed with `NotConnected` (29).
- `lore-revision`: A command whose remote refuses the caller fails with `NotAuthenticated` (16) or `NotAuthorized` (17), where it reported a missing branch, failed as an internal error (exit status 255) or went on without the remote. `lore branch merge into`, `lore revision restore` and `lore clone` of a repository with layers failed with 255. `lore branch merge` merged against the local latest revision of the branch, or failed with 255 when it had none. `lore branch diff` compared the branches' local latest revisions. A revision named by a branch's latest or by a number on it, such as `lore file reset --revision @LATEST`, resolved against the local latest revision of the branch. `lore link add`, `lore link update` and `lore history --branch` reported the branch as not found, and a three-way diff such as `lore file diff --diff3` failed with `InvalidArguments` (3), or on one branch with `Divergent` (42). `lore file obliterate` succeeded and left the remote copy in place, or, when the remote refused the obliterate request itself, failed after removing the local copy; it now fails before it removes anything. `lore branch create` failed with 255 when a linked repository or a layer refused the new branch, and `lore branch create --force` reported the branch as existing (57) when the remote refused the lookup of the branch to replace. Scripts that test these commands for 255 or a not-found code should test for 16 and 17 as well.
- `lore-revision`: A refusal while reading a revision's state is reported as the refusal, where it was an internal error. So is a refusal while `lore file diff --source` or `--target`, `lore file history`, `lore file write`, `lore file info` or `lore file reset` with `--revision` look up a revision on a branch the instance does not hold, while `lore file diff`, `lore file write`, `lore file obliterate` and `lore file info` look up the file, while `lore stage` looks up a path in the staged state, while `lore file reset` lists the staged links under its paths, while `lore file reset` and `lore file info --filtered` read the nodes below a directory, while `lore commit --link` looks up the link, and while `lore sync --root-file` looks up a root file. These reported the revision, the file or the link as not found, or failed with 255, and `lore file info` reported a file in the working tree as added.
- `lore-revision`: `lore file reset --last-merged-from` fails with the refusal when the remote refuses a revision of the file's history that the reset walks back through, or the merged-in parent of a merge on the way, where it reset the file to an earlier merge or to the branch point.
- `lore-revision`: `lore file audit` reports a line as link-unavailable when the remote refuses to read the state of its earlier version in a linked repository, as it does for that version's content. Such an audit failed with 255.
- `lore-revision`: A fragment's metadata read through a composite store whose durable store refuses the caller fails with that refusal, where it answered that the fragment was not there.
- `lore-server`: a `remote` immutable store trusts its upstream by default. It checks only the sizes of a payload it fetches, where it decoded and hashed every payload against its address. Clients still check each payload they fetch, except a client that trusts its remote (`--trust-remote`, or `trust_remote` or `skip_verify` through the C API), which checks only its sizes. A heal a client asks for now reaches the upstream too: the server drops its own copy of the payload only if that copy is corrupt, and asks the upstream to heal its copy where the server's own was corrupt or missing. A heal that arrives while the server writes a fetched copy of the payload to its local tier waits for that write, so it checks the copy the write leaves. To keep checking each payload on the server, set `verify = true` in the store's `remote` table.
- `lore-transport`: A gRPC request that the server or the authentication service answers with `Unauthenticated` reports `NotAuthenticated` (16) to its caller rather than an internal error (-1, exit status 255). The requests and commands these notes name fail with it, or with `NotAuthorized` (17) for `PermissionDenied`. A Lore server with `[server.auth]` sends `Unauthenticated` for a request without a bearer token, and `PermissionDenied` for a token it can't verify, such as an expired or malformed one. One with `server.trust_unsigned_identity` sends `Unauthenticated` for an unsigned token that doesn't decode. `NotAuthenticated` is a refusal of the credentials, as `NotAuthorized` is: a connection whose token exchange is refused this way is not held for its back-off, so the next call connects again at once. Both refusals keep the server's reason, where it gives one, on the error's trace. Scripts and C API callers that test one of these for 255 or -1 should test for 16 and 17 as well.
- `lore-transport`: A storage query, fragment verification or mutable-store request that the server answers with `Unauthenticated` or `PermissionDenied` fails with `NotAuthenticated` (16) or `NotAuthorized` (17), where it failed as an internal error. A push whose check for the fragments the server holds is refused this way fails with that code, and so do the mutable-store requests of a C API storage handle that reaches only the remote.
- `lore-revision`: `lore repository verify fragment` fails with `NotAuthenticated` (16) or `NotAuthorized` (17) when the server refuses the verification, where it failed as an internal error. Its fragment event reports the failure as before.
- `lore-revision`: A branch lookup that reaches the remote, as one for a branch the instance does not hold or one under `--remote` does, fails with `NotAuthenticated` (16) or `NotAuthorized` (17) when the remote refuses the connect or the query. A refused connect failed as `BranchNotFound` (87), a refused query as an internal error, and a lookup by id without `--remote` answered the id with no revision. `lore file history --branch`, `lore branch reset --branch`, `lore file reset --last-merged-from` and `lore branch metadata get`, `set` and `clear` with `--branch` fail with the refusal too, where they reported `BranchNotFound` (87), or `InvalidArguments` (3) for the metadata commands. Any other failure is reported as before.
- `lore-revision`: A fragment upload the server refuses fails with `NotAuthenticated` (16) or `NotAuthorized` (17), where it failed as an internal error. That is the upload of `lore push` (`lore branch push`), `lore branch merge into` and `lore revision restore`, and of a metadata blob by `lore branch metadata set` and `clear` and `lore repository metadata set` and `clear`.
- `lore-revision`: A read through a remote mutable store, as a server whose mutable store is a remote store makes, fails with `NotAuthenticated` (16) or `NotAuthorized` (17) when that store refuses it, and with the code of any other failure but a missing key, where it failed as an internal error. Its writes already failed this way.
- `lore-notification`: A subscription the server refuses as `Unauthenticated` or `PermissionDenied` fails at once with `NotAuthenticated` (16) or `NotAuthorized` (17). An `Unauthenticated` refusal failed as an internal error, and a `PermissionDenied` one was retried before it did.
- `lore-revision`: A notification subscription whose connect to the remote is refused fails with `NotAuthenticated` (16) or `NotAuthorized` (17), where it failed as an internal error.
- `lore-server`: A server whose immutable store is a remote store logs a request that store refuses as `Unauthenticated` as `NotAuthenticated` rather than as an internal error. Through a composite store, a fragment read the remote store refuses this way fails as a store failure, where the composite went on to its other stores, and its outcome metric counts the refusal as `not_authenticated`.

### Features

- `lore-client`: an `[auth]` table in the user-level `config.toml`. `store = "fallback"` keeps the token store's key in a file its owner alone can read instead of the system's secure store, for machines where nothing can answer a secure-store prompt, and `store = "secure"` keeps it in the secure store; any other value is an error. `path` moves the token store, a relative `path` is relative to the directory of `config.toml`, and `fallback` needs one, or `LORE_AUTH_PATH`. `LORE_AUTH_STORE` and `LORE_AUTH_PATH` override them with the same values, and a blank variable is unset. Only the `lore` command applies the table.
- `lore-credential`: on Unix, a new token store directory is its owner's alone.
- `lore-revision`: `[store] cache_payloads` in an instance's config sets whether the content of files fetched from the remote, and of files written, is kept in the store.
- `lore-server`: `[server.auth.oidc] client_credentials_scopes` names the scopes clients request when they log in with the client credentials grant, for a provider that adds the claims Lore reads only for a requested scope.
- `lore-transport`: `lore auth login --client-id` requests the scopes the server names in `client_credentials_scopes`, and no scope when it names none. Older clients request no scope whatever the server names, so CI logins against such a provider need this client.
- `lore-server`: `[immutable_store.composite] read_replica_selection` chooses which read replicas a read that misses the local store is sent to. `fan_out`, the default, sends it to every read replica. `round_robin` tries read replicas one after another, from the next position in rotation: up to `round_robin_max_tries` of them (default 2), each for at most `round_robin_try_limit_ms` (default 50) before the next starts, and at once when one misses, partially matches or fails. A try past its limit can still answer the read. The durable store is read once every try has ended without an answer, or when `durable_store_delay_ms` passes. The server logs these settings at startup.
- `lore-server`: The repository content and presigned redeem endpoints serve the single byte range a `GET` request's `Range` header asks for with `206 Partial Content`, and answer a range that selects no byte of non-empty content with `416 Range Not Satisfiable`. A request with several ranges, or with `If-Range`, gets the whole content. `200`, `206` and `416` responses carry `Accept-Ranges: bytes`. A `HEAD` request to the repository content endpoint returns its headers without streaming the content.
- `lore-client`: `--fast-flush` (C API: `fast_flush` in `lore_global_args_t`) has the command's flushes write the local stores' index in place rather than replacing each of its files through a temporary file, which is faster where creating files is slow, as on Windows. If the process stops or a write fails while a file is being written, the store drops every entry that file held, including those of revisions and branches not yet pushed. Eviction and compaction still replace each file they write, and `--sync-data` still syncs what is written.
- `lore-client`: `lore repository instance clone <target-path>` clones an instance on a shared store to a new path as an instance of its own, on the source's revision, branch and staged state, with its uncommitted changes. On the source's volume, on a filesystem that clones files (APFS, Btrfs, XFS, bcachefs, ReFS and Dev Drive), the clone shares the source's data blocks and takes little space. Elsewhere the clone is refused with what a copy would cost, unless `--allow-copy` is given and the destination has room. `--dry-run` prints how the clone would be made. The clone's first `lore status --scan` hashes only the files a scan of the source would: those with no modified-time record, and those changed since theirs. While the clone runs, commands that open the store, on the source instance and from other processes on every instance over the store, wait for it. So does `lore repository instance delete`, which opens the store to deregister the source before it removes any file. `lore repository instance move` renames the source before it opens the store, so moving the source while it copies makes the clone fail; on Windows the move is refused while the clone lists a directory of the source. A clone that fails removes what it copied before the command returns. A repository nested inside the instance is left out. A destination that exists or lies inside an instance, the source included, is refused, and so are instances with a local store and virtual clones. A clone that is interrupted can leave a directory beside the destination, which the next clone to the same destination removes.
- `lore`: the C API adds `lore_repository_instance_clone` and its `_async` form, taking a `lore_repository_instance_clone_args_t` with a `target_path` and an `allow_copy` flag, and the `LORE_EVENT_REPOSITORY_INSTANCE_CLONE_BEGIN`, `_PROGRESS` and `_COMPLETE` events. Failures report code 9 (not supported), 3 (invalid arguments), 4 (invalid path), 118 (oversized) or 82 (file not found), as `lore.h` describes for each case.
- `lore-revision`: an instance reads its own ignore rules from `ignore` in its Lore directory (`.lore/ignore` for a regular instance), in the syntax of `.loreignore`, after the rules of `.loreignore`. Like `.loreignore`, they apply only to paths the tree doesn't track yet, and are never committed. They can only exclude: a `!` line is skipped with a warning naming it. A file that cannot be decoded fails the filter load with an error naming the file.
- `lore-transport`: a `lore-unix://<upstream host>/<repository>` remote reaches a machine server on the same machine through the socket `LORE_UDS_SOCKET` names, which a container can have bind-mounted. Storage requests travel on one connection per process, answered in any order, and the client opens its sessions again on a new connection when one closes. A `lore-unix` remote without the variable fails with an error that names it.
- `lore-transport`: with `LORE_UDS_SOCKET` set, every gRPC channel the process opens, the authentication service's included, goes through the socket in plaintext, naming the host it would have dialed, so the process reaches only the hosts the machine server forwards to. OIDC login requests still go to the network. The Lore service reads the variable from its own environment, not from the commands it carries out.
- `lore-server`: `ThinClientService.RevisionTree` attributes each entry to the revision that last changed it when the request sets `include_last_revision`, and the header's `last_revision_included` says so. The entry's `last_revision_index` names a `TreeRevision` the stream sends before it, carrying that revision's signature, identifier, number, commit message, timestamp and committer, and is 0 for an entry the server can't attribute. An entry under a link is attributed in the linked repository. A read for attribution that the store refuses as overloaded ends the stream with `RESOURCE_EXHAUSTED`. Without the flag, no entry is attributed and no `TreeRevision` is sent.
- `lore-server`: `ThinClientService.RevisionTree` names the repository each entry's content is in, as `RevisionDiff` does: a link, and an entry in a linked repository, carry in `link_repository_index` the index of a `DiffPartition` the stream sends before it, and an entry in the requested repository carries 0. Each `TreeRevision` names the repository its revision is in the same way.
- `lore-server`: `[server.uds]` serves the containers of a build machine over sockets in the filesystem, one per container, listed in `sockets`: Unix domain sockets on Linux and macOS, and `AF_UNIX` sockets on Windows. A socket serves storage requests from the server's stores with the same checks as the public QUIC endpoint, and forwards gRPC requests unchanged to the hosts `[server.uds.forward]` names, refusing any other host with `PERMISSION_DENIED`. `credits`, `credit_bytes`, `max_connections_per_socket`, `socket_bytes` and `handler_timeout_seconds` bound each connection and each socket, and `socket_mode` sets who may connect. The table takes effect with `enabled = true`, which requires `[server.auth]`, an authorizer that grants per partition, at least one socket, and `forward` keys that are host names alone, without a scheme or port; an empty `forward` starts with a warning. The server claims each socket with a `<socket>.lock` file beside it, refuses a socket that another server has claimed or answers on, and closes every socket connection at the shutdown signal.
- `lore-server`: the server refuses to start with `LORE_UDS_SOCKET` set in its own environment, which would send its own gRPC into a machine server's socket.
- `lore-server`: `server.upstream_connections` sets how many storage connections the server opens to the upstream of a `remote` immutable or mutable store, from 1 to 10. Every repository the server reaches there shares them, for QUIC, gRPC and socket requests alike. It defaults to 4; a server opened one before.
- `lore`: `lore service run`, and a program that runs the service through the C API, close a connection to a remote once no command has used it for five minutes, so the server releases it. Until then the next command reuses it without connecting again. Set how long in seconds with `idle_connection_timeout` under `[service]` in the user-level `config.toml`.
- `lore-client`: `lore clone --vfs fuse` makes a virtual clone on Linux: the Lore service mounts the repository with FUSE and serves its files from the shared store for its remote, fetching them on first read, so the clone writes no files. A command that makes one, or works in one, runs through the service, which mounts its virtual clones again when it starts. With no service running, a command that makes one starts the service, as does a command in one whose service ended without taking its mount down: from the executable `lore service set-executable` names, or with none named, from the `lore` that ran the command. A command in a virtual clone whose service is still starting, or listens on another socket, starts none, and says which socket it looked on. `--vfs default` picks FUSE on Linux, SWFS on Windows and no virtual filesystem elsewhere, where it picked SWFS on every platform
- `lore`: `LORE_VFS_TYPE_FUSE` asks `lore_repository_clone` for a virtual clone on Linux, and `LORE_VFS_TYPE_DEFAULT` picks FUSE on Linux, SWFS on Windows and none elsewhere
- `lore`: `lore::serve_from_this_executable()` tells the library that the program's own executable runs the service with `service run`, so starting the service, and a command that makes a virtual clone, start one from it when no executable is named
- `lore-revision`: Virtual clones run the edit loop as an ordinary clone does, the ignore file included, apart from a file added with `lore file dirty` that the ignore file names: it stays marked, where an ordinary clone's scan drops it. A virtual clone resolves names in the case they are spelled, as an ordinary Linux clone does, so a rename that changes only case, such as `mv Foo foo`, is seen and staged as a move. A file or directory whose name differs only in case from one beside it cannot be created there, since a repository holds one spelling of a name. `lore status`, `lore stage` and `lore diff` see every write through the mount whose system call returned, with no `--scan` and no `lore file dirty`. A plain `lore status` lists them as it lists marked changes on an ordinary clone, a deleted directory as one entry; `--scan` adds the entries below it and a summary. `lore commit` stores the content the mount serves, then serves the committed files from the store, each keeping the file number `stat` reports and its modification time. Deletions through the mount are recorded without syncing to disk, so a crash may bring back the last ones. Symbolic links are not versioned, as on an ordinary clone: status and commit take one as nothing there, and `lore stage` naming one skips it, where an ordinary clone stages the file it names. Writes still meet one: a sync, merge or cherry-pick that would write where a symbolic link stands, or below one, stops as for a local edit
- `lore-revision`: `lore sync` and `lore branch switch` on a virtual clone check local changes first, as on an ordinary clone: an edit of a file the target changes stops the command with exit code 44 unless `--force` is given, which gives the file the target's content, and `--dry-run` stops after the check. A file deleted locally that the target changes is restored, unless a symbolic link stands at its path. There `--force` keeps the symbolic link: the target's change at that path is not applied, and status shows the tracked file deleted until `lore reset <path>` replaces the link with the target's file; `--forward-changes` keeps the link too, dropping the target's change at that path. The target is then applied to the mount in place, so directories it leaves alone keep their cached listings
- `lore-revision`: `lore reset` and `lore branch merge` on a virtual clone, and a `lore sync` that merges, write their files into the mount. A reset back to the mounted revision serves the file from the store again; other files they write stay local until the next commit stores them. A reset removes a symbolic link standing at a path it writes and puts the file in its place, never writing through the link. A conflicting merge writes the file with conflict markers beside its side files, as on an ordinary clone
- `lore-revision`: `lore clone --vfs fuse --prefetch <file>` warms the local store in the background from an access recording of up to 128 MiB after the clone returns: the recorded directories' node blocks and listings and the recorded files' contents. `LORE_TREE_PREFETCH_DEPTH` in the service's environment selects the tree prefetch that follows: `blocks` (default), a level count, `all` or `0`
- `lore-revision`: FUSE mounts record the first access to each path their revision holds, with each directory's node block indices, to `<dir>/<instance id>-<unix seconds>.txt` when the service's environment sets `LORE_FUSE_RECORD_DIR=<dir>`, as input for `--prefetch`
- `lore-revision`: `lore repository instance move` and `lore repository instance delete` work on a virtual clone. Moving one unmounts it and mounts it at the target at the revision it served, with its uncommitted changes; if it cannot be mounted there, it is mounted at the source again. The old mount point is removed once the instance is registered at the target, so a service that starts in between still finds it. Deleting one unmounts it before anything else, then deregisters it and removes its external directory and the empty mount point. Both refuse a mount something holds, such as a shell whose working directory is inside it, saying the mount is busy and leaving the instance as it was. A move is also refused while the service stops

### Fixes & Improvements

- `lore-server`: in a composite immutable store, a read that arrives while the server writes a fragment it fetched from the durable tier to the local tier takes the fetched copy instead of fetching the fragment again.
- `lore-server`: Lore Server's DynamoDB mutable store answers a compare-and-swap whose new value is the value it expects with a strongly consistent read, which writes nothing, instead of a conditional write. The operation-latency metric times such a swap as `get_item`, and a swap to zero as `delete_item`, where it timed both as `put_item`, and its `delete_item` series gains a `conditional` label.
- `lore-revision`: `lore repository instance delete` renames the directory that holds an instance's files, the instance directory or an SWFS instance's external directory, aside to `<name>.deleting-<instance ID>` beside it before deleting the files, so a regular instance's path is free as soon as the rename lands, and deletes the files at background disk priority. A delete that fails part way leaves what remains under that name, and the error names it. A delete refuses to start when that name is already taken. An instance directory that is a mount point, which refuses the rename, is deleted in place. On Windows, an instance directory that is a volume mounted in a folder or a junction is not renamed: its files are deleted through it, and the mounted folder or junction is left in place. A volume mounted in a folder also keeps the hidden system entries at its root, such as `System Volume Information`.
- `lore-revision`: An instance keeps the modification times it records for its files in `.lore/records/` instead of the shared mutable store, so instances on one shared store no longer load and rewrite each other's times. A copy of the instance's directory that keeps its files' modification times carries times that describe its files, so the copy's first scan skips re-reading them. An instance created by an older client still uses the times it kept in the store until it records new ones. SWFS and in-memory instances keep their times in the store.
- `lore-revision`: `lore repository instance move` creates the directories above its target that don't exist, where it failed with `Target path parent does not exist`. It creates them only once every check has passed, so a dry run or a refused move creates nothing, and a move that fails or is undone removes them again. A target inside the instance is refused before anything changes, where the move failed at the rename
- `lore-revision`: a failed layer push names the layer (its path, repository and branch) and keeps the reason, as a failed link push does. A failed push of a link inside a linked repository names its full mount path.
- `lore-revision`: `lore push` pushes the branches of the repository, its layers and the repositories they link up to 8 at a time, and each linked branch once, at the newest revision linked, rather than once for every revision that links it. A branch is pushed once the branches it links have been, and the fragment queries and uploads of every branch share one bound.
- `lore-revision`: a link push that fails no longer stops the pushes of links it doesn't hold back. The push fails with the first failure in the order the links were reached, and reports each other one on a line of its own. A link that can't be planned, such as one whose revisions are on diverged lines, fails the push before anything is pushed.
- `lore-revision`: a push whose repository or layer has diverged from the remote fails before it pushes anything.
- `lore-client`: a command runs one async thread fewer than the processor count, which leaves a processor for its 2 network threads. A command that opens several storage connections, with `--max-connections` or `LORE_MAX_CONNECTIONS`, runs a network thread for each of them, up to half of its network and async threads together, and as many fewer async threads, so it runs as many threads as with one connection. On 8 processors, a command runs 7 async and 2 network threads, and with 10 connections 5 async and 4. `lore service run`, which serves many commands at once, keeps its async threads and adds the network threads 10 connections get: 7 async and 4 network threads on 8 processors. `LORE_NET_THREADS` still sets the network thread count, with one async thread per processor, and `LORE_MAX_THREADS` still caps the total.
- SWFS: a release build links no dynamic C runtime, as the SWFS library no longer asks for one
- `lore-server`: The replicated immutable store opens `num_clients` connections to the remote server (default 2). Each request uses the healthy connection with the fewest requests of its kind in flight, then the fewest in total, so requests stuck on a stalled connection steer new ones to the others. The connections refresh at staggered times, and one whose reconnect fails keeps retrying until it is back. `client_message_limit`, `max_bandwidth_bytes_per_second` and the other client settings apply to each connection, so the default doubles the bandwidth and the requests in flight an edge server can use toward the remote. Set `num_clients = 1` to keep one connection. Client connection metrics and the new `client.inflight_requests` gauge carry a `client_slot` label.
- `lore-server`: the local immutable store now uses each group's delayed flush to write index buckets changed by a capacity eviction pass. These writes are spread across one `flush_delay_seconds` interval instead of happening for every group at once when the pass ends. A pass that evicts from every group no longer queues all of their bucket writes on the I/O pool at once, ahead of payload reads and writes. If the server stops before a group's buckets are written, the entries evicted from them are back when it restarts. Servers in maintenance mode still write when the pass ends.
- `lore-storage`: `WriteOptions` gains `keep_local`, with `with_keep_local` and `cache_local`, so a struct literal names it: the write keeps its payload in the writing store once it is durable on the remote, as `local_cache_priority` does, without recording it in the fragment for every store that fetches it to cache
- SWFS: an instance keeps the content it commits in its local store when the commit uploads it, so a sync back to that revision serves the files, where the mount served them as zeros once the store had dropped the uploaded payloads. Content writes follow the repository's caching setting, as reads do, so a repository run with `--cache` keeps what it uploads as well
- `lore-client`: `lore clone` and `lore repository create` with a `--vfs` other than `none` use the shared store for the remote as if `--use-shared-store` were given, and take `--shared-store-path` without it. An SWFS clone made without the flag failed with an internal error
- `lore`: `lore_repository_clone` and `lore_repository_create` use a shared store for a virtual clone where `use_shared_store` is `LORE_SHARED_STORE_MODE_INHERIT`, whatever the machine's setting, and refuse `LORE_SHARED_STORE_MODE_DISABLED` for one with `InvalidArguments`
- `lore-storage`: Fixed content missing from a local store after a read that caches it, when another read was fetching the same content at the same moment: one into another local store in the same process, such as two virtual clones over different shared stores in one service, or one into the same store that does not cache it, such as a prefetch stopped before it stored the content. The read returned the content without storing it, so a later offline read of it failed.
- `lore-storage`: Fixed a file staying unreadable from a local store after eviction or compaction removed its content's payload, when that content was fetched again for another file that shares it.
- `lore-storage`: Fixed a commit failing with "leader upload failed", and a storage put or upload (`lore_storage_put`, `lore_storage_upload` and their variants) completing without the content in its own local store, when another local store in the same process was writing the same content to the same repository or partition at the same moment, such as two instances of one repository in one service, each with a store of its own, or two in-memory storage handles.
- `lore-revision`: Fixed a push, sync or revision lookup, in its walk back to the newest revision the local and remote latests share, finding an older shared revision when the local line reaches the newest one before the remote line does, which could report a branch the remote is ahead of as divergent.
- `lore-revision`: Fixed that walk asking the remote again, on every step, for a line the remote had already ended.
- `lore-revision`: Fixed that walk reading a failed request for a line's revisions as the end of the line, which could report lines that share a revision as divergent. A walk that can get no further fails with the remote's error, such as `Disconnected` (28) or `NotAuthorized` (17).
- `lore-revision`: Fixed that walk asking the remote for revisions the local store holds once a remote page led it to one, which cost a round trip per page. It also no longer asks the remote in a step where the two lines already meet among the revisions it has read, or both reach the same revision the local store lacks.
- `lore-revision`: Fixed that walk looping without end on a remote whose pages return to a revision they already listed, or don't move the line at all. The first fails the walk with an internal error, as does a line whose revision numbers don't decrease, or a page the walk needs that lists a revision on the line without its own state. The second stops the line, so a walk that can get no further fails with `RevisionNotFound` (88) naming the revision it stopped at. A revision lookup whose walk fails answers the local latest.
- `lore-revision`: Fixed that walk taking time that grows with the product of the two lines' lengths, which slowed a push or sync of a branch far from the remote's latest. It now takes time that grows with the revisions it walks.
- `lore-revision`: Fixed a revision lookup by number trusting the number a remote page lists beside a revision, which could resolve it to a revision with another number. It takes the listed revision only where the revision's state carries the number, and otherwise searches the history.
- `lore-revision`: Fixed a three-way merge or diff taking a base older than the newest revision the two lines share when one line reaches that revision a batch before the other, which can report conflicts the newest base wouldn't. A line whose revision numbers don't decrease fails the search for that base with an internal error, as does a page that lists a revision on a line without its own state. Such a page also fails the search for the layer revision whose metadata matches a revision.
- `lore-revision`: Fixed the first commit on a branch that `lore branch create` made after `lore link update`, or over nested links, failing with `Branch has been advanced by another instance`. A link moved to a revision on its parent's branch keeps following that branch, so the new branch needs no revision to repin it
- `lore-revision`: Fixed `lore branch create` over a link pinned to another branch leaving the working copy behind the new branch, failing with `Failed to deserialize revision metadata` when changes were staged, and leaving a link that a staged change removes or remounts on its old branch
- `lore-client`: Fixed `lore link update` on a link whose repository has not moved printing only `Link is already up to date`. It names the link and says it was not pinned
- `lore-revision`: Fixed concurrent creates, with `lore branch create` or the C API, of one name, or of one new branch id under two names, all succeeding, which left a branch without its name. One now succeeds, and the others fail with `BranchAlreadyExists` (57). A create that loses the name removes the metadata it wrote.
- `lore-revision`: Fixed restores and archives of one branch that race undoing each other's writes. Concurrent creates that restore an archived branch by its id all succeeded, and left it mapped from each of their names. A restore under a new name could rename a branch whose create hadn't finished. `lore branch archive` could release the name of a branch a concurrent restore had renamed, which left it live, and a failed restore could remove the name another restore took. Of racing restores at most one now succeeds, and the others fail with `BranchAlreadyExists` (57), as does a restore that would rename an unfinished branch. A branch without a latest revision that an earlier version archived is still restored under its old name. An archive archives the branch as a concurrent restore leaves it. The next create or archive of the branch, or `lore branch create --force` of the name, settles a restore that stopped part way, as when its process ended, and frees the name it claimed. Branch metadata gains `archived` and `restored`, times in milliseconds since the Unix epoch, `restoring`, the name a restore in progress claims, and `released`, the name a settled restore claimed. An archive and a restore each write a metadata blob to the immutable store, as a create does.
- `lore-revision`: Fixed writes that undid a concurrent writer's change. A restore wrote its metadata over a protect or a metadata edit made while it ran. `lore branch protect` and `lore branch unprotect` without a remote put back the old name of a branch a concurrent restore renamed, after which an archive of it failed with `BranchNotFound`. A clone overwrote the name of a branch it records that another writer had claimed. `lore branch archive`, and `lore branch create --force` replacing a branch, removed a name mapping a concurrent writer had pointed at another branch. Each now keeps the other writer's change: the clone logs a warning, and the forced create fails with `BranchAlreadyExists` (57).
- `lore-revision`: Fixed a create or a clone taking a read that failed as one that found nothing. A create wrote fresh metadata over the branch whose id it was given, or took a name from the branch it maps to, and a create given an archived branch's id took the branch's old name as free. A clone recorded a name whose mapping it couldn't read. They now fail with the read's error, a remote refusal included, so `lore branch create` fails while the remote refuses the caller. A create whose remote can't be reached still succeeds. `lore branch archive` of a branch only the remote holds now fails with the remote's refusal, where it failed with `BranchNotFound`.
- `lore-revision`: Fixed a create given an archived branch's id being refused when the branch's old name now names another branch. It restores the branch under the new name. Fixed `lore branch create --force` of a name mapped to a branch that records neither the name nor a restore under it archiving that branch and then failing. It now takes the name and leaves the branch as it was.
- `lore-server`: Fixed the branch delete, the branch list's repair of a missing default-branch mapping, and the repository delete changing name mappings other writers had made. A delete removed a mapping a concurrent writer had pointed at another branch, the repair overwrote a name another writer claimed first, and a repository delete left behind each name that maps to a branch whose metadata names it otherwise. Of two concurrent deletes of one branch only one is now counted, notified and runs the post hook, and a repository delete releases every name mapping.
- `lore-server`: Fixed a branch create dropped part way, at its deadline or by its client, leaving an archived branch half restored. A create and a delete now run to their end, and one dropped is still counted, notified and runs the post hook. Fixed the branch create answering a store failure, such as a failed read of the name's mapping or of a branch's metadata, as `INVALID_ARGUMENT`, and logging nothing. It now answers `INTERNAL` and logs a warning.
- `lore-server`: Fixed the branch create making a branch with id zero for a request whose id is zero or unset. It now answers `INVALID_ARGUMENT`.
- `lore-server`: Fixed the deprecated `urc.rpc` `BranchProtect` and `BranchUnprotect` requests putting back branch metadata that a restore changed while they ran.
- `lore-revision`: Fixed `lore branch merge`, `lore push --fast-forward-merge`, `lore revision cherry-pick`, `lore revision revert` and `lore sync` reporting a conflict where one side renames a directory only in case and edits a file below it, and the other side moves that file. The merge now takes the rename, the move and the edit: the file stands where the move put it, with the edit, and below the directory's new spelling where the move keeps it in the directory.
- `lore-revision`: Fixed `lore branch merge`, `lore revision cherry-pick` and `lore revision revert` leaving a second spelling of a directory in the working tree where one side renames the directory only in case and edits a file, and the other side moves that file into the directory.
- `lore-revision`: Fixed `lore branch merge`, `lore push --fast-forward-merge`, `lore revision cherry-pick`, `lore revision revert` and `lore sync` where each side renames a directory only in case, one directory inside the other, and one side edits a file below both. Where the other side leaves the file, the edit stood below the inner directory's old spelling, and a merge into the working tree left both spellings of it. Where the other side moves the file, the merge reported a conflict. The edit now stands below both new spellings.
- `lore-revision`: Fixed the same commands reporting a conflict where both sides rename a directory or a file only in case to the same name, or move a file to the same place, and one side also renames a directory above it only in case. The two renames, or the two moves, are one.
- `lore-revision`: Fixed `lore branch merge`, `lore revision cherry-pick` and `lore revision revert` where both sides rename a directory only in case to the same name, and the merged side moves or deletes a file below it, or renames a directory below it only in case. The move left the file at its old path beside the moved copy, the delete left the file, and the rename left the directory as it was, with an empty one of the new name beside it. A move, a delete or a rename only in case of a file or a directory below such a rename now takes its node from where the working tree holds it.
- `lore-revision`: Fixed the same commands taking one of two files added at one place below directories renamed only in case, however the two sides split the renames, with no conflict. The two adds now conflict.
- `lore-revision`: Fixed the same commands reporting a conflict where one side renames a directory only in case, deletes a file below it and adds it again with the same content, and the other side edits or moves the file. The delete and the add are no change, as where no rename comes with them.
- `lore-revision`: Fixed `lore branch diff`, `lore status` and the server's diffs listing a link twice where it is renamed only in case, and either given another pin or mount where the diff doesn't descend into it or staged with nothing staged below it.
- `lore-revision`: Fixed `lore commit` dropping a file that `lore file dirty move` moved into a new directory only marked dirty: the commit took in the move of a file not staged, and discarded a staged one with the directory. A commit takes only staged changes: a moved file not staged stays where it was committed, the move stays marked dirty after the commit, and a commit with nothing else staged finds nothing to commit. A directory moved with `lore file dirty move`, or added and only marked dirty, that holds a staged change, or held one when it was staged, is committed where it stands, with its committed contents and its staged changes. Where a staged change displaces a move not staged, such as a new file staged at the moved file's committed path or the deletion of its directory, the commit warns that it does not keep the move, and the file at the moved path is marked dirty as added.
- `lore-revision`: Fixed `lore commit` dropping a committed empty directory that was only marked dirty, such as one holding a new file marked dirty but not staged, or one marked dirty as deleted.
- `lore-revision`: Fixed `lore file dirty` marking a file moved with `lore file dirty move` as modified, losing its move, where the revision holds another file at its new path.
- `lore-revision`: Fixed `lore file dirty` of a directory marking a node that `lore file dirty move` moved out of it, or renamed in it, as deleted, losing the move.
- `lore-revision`: Fixed `lore file dirty` of a file that `lore file dirty move` renamed to its own name in another case marking it modified, losing the rename, which a commit then took though it was not staged.
- `lore-revision`: Fixed `lore file dirty` of an edited file that `lore file dirty move` moved leaving it marked as moved alone, and `lore file dirty move` of a file marked modified dropping that mark: such a file is marked moved and modified.
- `lore-revision`: Fixed `lore stage` of a path inside a directory moved with `lore file dirty move` staging the directory as an added one, after which `lore commit` marked every file in it modified. Such a stage stages the move of the whole directory, as `lore stage move` does. A change of a file's own inside it stays dirty until it is staged: an edit marked dirty, a move or rename, an add or a deletion. Past the commit, the files moved with the directory carry no mark, and `lore status` lists only those changes.
- `lore-revision`: Fixed `lore commit` failing with a non-zero exit status after it stored the revision, where marking the dirty changes again or carrying a merge's dirty changes failed. It warns and exits with status 0, and the changes it could not mark stay unmarked until `lore status --scan` finds them.
- `lore-revision`: Fixed `lore commit` dropping a committed file that `lore file dirty move` moved into a directory only marked dirty as added, once `lore file dirty move` moved that directory too.
- `lore-revision`: Fixed `lore commit` storing a revision whose directory addresses did not match what they hold: where a staged file had been moved with `lore file dirty move` out of a directory nothing staged, and where a directory only marked dirty as added had lost that mark, as marking or staging the removal of the file added in it took it off. Such a directory keeps its mark once emptied, and `lore reset` of the file takes it out of the staged tree with the file.
- `lore-revision`: Fixed `lore file dirty`, and the dirty marks a sync, switch or reset carries over, adding a directory below a file in the staged tree for a path below a committed file that the working tree holds as a directory. Such a path is not marked. For each tree it marks, the command warns once, naming the first such path and the file and counting the others.
- `lore-transport`: Fixed a stored login that has expired and can no longer be refreshed failing with `NotAuthorized` (17) instead of `NotAuthenticated` (16). A refused repository authorization still fails as `NotAuthorized`. An unreachable auth service, or a refresh that could not be sent, fails as `Disconnected` (28) and keeps the refresh token.
- `lore`: Fixed `lore auth info` and `lore auth logout` failing on such a login. `auth info`, and `lore_auth_local_user_info` and `lore_auth_user_info` without an auth endpoint, refresh an expired login first, unless the call supplies a token.
- `lore-transport`: Fixed a connection to a remote that had failed, or that another command connecting to the same remote at the same moment had replaced, staying open with its keep-alives until the process exited. A process that runs many commands, such as `lore service run` or a program embedding Lore, kept each such connection's storage connections open.
- `lore-server`: Fixed `lore push --fast-forward-merge` storing a merged revision whose directory hashes did not match the files below them. An instance that synced from such a revision to an earlier one kept the merged changes in its working tree and reported none, and a revision committed on top of one recorded the merged directories as changes of its own. Revisions an earlier server merged keep the directory hashes they were stored with.
- `lore-revision`: Fixed `lore file dependency add`, `remove` and `list` on a path inside a link acting on an unrelated file of the parent repository. They now fail with "Operation not supported" and change nothing. A `--root-file` inside a link in `lore clone` or `lore sync` is skipped with a warning instead of selecting unrelated parent files, and the command fails with "Operation not supported" when every root file is inside a link. The `LORE_EVENT_DEPENDENCY_RESOLVE_BEGIN` event counts only the root files kept, and is sent once they are resolved.
- `lore-revision`: Fixed `lore sync --root-file` also updating files inside a link that were not in the dependency set.
- `lore-revision`: Fixed `lore sync --root-file` failing with "file not found" for a root file spelled as `lore clone` accepts it, such as `./a.txt` or `dir\a.txt`.
- `lore-revision`: Fixed `lore file metadata` on a path inside a link changing or reading an unrelated parent file. `set` and `clear` now refuse such a path, and `get`, `list` and `lore file dump` read the linked file.
- `lore-revision`: Fixed `lore file metadata set` succeeding without setting anything on a missing path or past the 1 MiB metadata limit.
- `lore-revision`: Fixed `lore --force stage <directory>` staging only what was marked dirty below the directory, so it staged nothing for a committed directory deleted from the working tree. A forced stage of a directory walks it as `--scan` does and stages everything in it but Lore's own entries, past the ignore file and the view: unchanged files are restaged, and what the working tree no longer holds is staged deleted, except what the view leaves out. `lore --force stage .` stages the whole working tree. The same holds for a forced stage through the C API.
- `lore-revision`: Fixed `lore repository create --force` and `lore clone --force` at the path of a mounted FUSE virtual clone, or a path inside one, deleting the clone's Lore directory and then failing, which left the clone unusable. They now fail with `RepositoryAlreadyExists` (58) before changing anything, as they already did for an SWFS virtual clone.
- `lore-revision`: Fixed `lore --force dirty` in a sparse working tree marking the deletion of committed files the view leaves out. A forced dirty mark asks the view about a deletion, as `lore --force stage` does: for a directory or a file named, and below a directory removed from the working tree, it marks only the deletion of what the view keeps.
- `lore-revision`: Fixed content a forced stage takes past the view. `lore commit` failed with "Node with zero hash remain after nodes were committed" after `lore --force stage --scan` staged a new file the view excludes, and committed the revision's content in place of an edit there. The commit reads such a file from the working tree, and `lore status` lists it as staged. `lore unstage` follows the view and leaves it staged; `lore --force unstage` takes it back. Below a layer mount, `lore unstage` doesn't follow the view yet.
- `lore-revision`: Fixed `lore --force stage` of a file a merge in progress staged past the view. The merge commit recorded the merged content with the size of the working tree's file. It takes the working tree's file, its content and size, as the merge's resolution, and records it as merged, as it does for an edit of a merged file in view.
- `lore-revision`: Fixed `lore status` leaving out what a merge in progress staged at paths the view excludes. A path where the merge took the incoming side shows as a staged change with that side taken.
- `lore-revision`: Fixed `lore status` listing a file a merge took without a conflict as an unresolved conflict after `lore branch merge resolve theirs` or `resolve mine` on a directory holding it. It is listed as a staged change with the side taken, and the status event of the C API carries `flag_conflict_unresolved` unset for it.
- `lore-revision`: Fixed `lore unstage` of a directory added in the stage taking back what the view excludes below it. Where the view could take in something below the directory, as `!*/Saved/Config/` can below every top-level directory, the unstage took back every file below it. It takes back only what the view keeps, and the directory stays staged while anything below it does.
- `lore-revision`: Fixed `lore --force stage`, or a stage of an unresolved conflict, of a file staged as a move, or staged as an add by a merge. The commit recorded it as a modification. It keeps the move or the add, with the working tree's content.
- `lore-revision`: Fixed `lore unstage` of a directory or a link holding a change past the view, which the unstage leaves staged. The commit recorded the directory as the current revision holds it, without the change. The commit left out the change below a link, and status no longer listed it. `lore unstage .` took the change back where nothing else stayed staged or dirty. The directory or link stays staged while anything below it does.
- `lore-revision`: Fixed a same-size edit on a FUSE mount, made right after a sync or branch switch changed the file, being missed by `lore stage` and `lore status`.
- `lore-revision`: Fixed commands that change a setting in the user-level `config.toml` dropping the keys and tables they don't know, such as a setting that a newer version of Lore added. These include `lore service set-executable` and a clone that records a default shared store.
- `lore-revision`: Fixed a push, sync or revision lookup, in its walk back to the newest revision the local and remote latests share, reading a stored blob that isn't a revision state as a revision's number and parent where a latest or a parent names it, which could answer a wrong branch point. The walk fails with an internal error, whose trace names the revision, and a revision lookup whose walk fails answers the local latest.
- `lore-revision`: Fixed the search for the base of a three-way merge or diff taking such a blob for a revision on the line. The search stops reading the local store there, as it does at a revision the store lacks.
- `lore-revision`: Fixed the search for the layer revision whose metadata matches a revision. It never returned where the layer's line left the local store, at a revision the store lacks or at such a blob, and no remote listed it from there. It now compares the layer revisions the store holds, and fails with an internal error only where none matches. With `--search-nearest`, where the line of the revision being matched leaves the store in the same way, it compares that line's revisions the store holds, rather than only the revision itself. Without `--search-nearest`, a search that finds no match also fails where the layer's line ends, instead of after the search limit.
- `lore-revision`: Fixed a three-way merge or diff taking an older revision as its base, or failing as divergent, where a history line it follows runs past the first hundred revisions and leaves the local store further down, and no remote lists the line from there. The search for the base follows the line as far as the store holds it. Where the remote serves a line's revisions but fails to list them, and the store holds the revision the line starts from, the search no longer reads them one at a time past where the line leaves the store.
- `lore-revision`: Fixed `lore repository instance move` and `lore_repository_instance_move` moving an instance into another repository's working tree, or into a virtual instance's mount. Such a target is refused with code 3 (invalid arguments), as a target inside the instance itself is. Creating or cloning an SWFS instance inside a FUSE instance's mount is refused the same way.
- `lore-revision`: Fixed `lore branch merge start` checking paths inside a link at the repository root, not the mount. A local edit or untracked file in the way there is refused with `LocalModifications` before the merge starts.
- `lore-revision`: `lore branch merge start` checks every link, the main repository and carried link pins first. Local work in any of them refuses the whole merge before anything is merged, leaving no merge to abort.
- `lore-revision`: Fixed `lore link update` refusing every link with `Link has filesystem changes` while the parent had anything staged. It refuses where `lore sync` refuses, where realizing the new pin would overwrite or delete a local change, whatever the parent has staged.
- `lore-revision`: Fixed `lore lock acquire`, `lock status`, `lock release` and `lock query` skipping a committed file the ignore file names unless `--force` was given, and `lore file info` leaving such a file out of the local and filtered sizes it reports. As in status and staging, the ignore file applies only to paths the tree doesn't track. `lore file info` measures against the staged tree, or the revision `--revision` names, so a staged ignored file counts toward its sizes, also below a link mount, and a named untracked file the ignore file names is left out of the local size.
- `lore-revision`: Fixed `lore lock query --path` without `--branch`, and `lore_lock_file_query` with an empty `branch` and a `path`, failing with "unsupported lock query combination". A path given without a branch is queried on the branch the instance is on. A query combining `--owner` with `--path`, which the server answers for no branch, is refused before the request is sent, with the invalid-arguments error.
- `lore-revision`: Fixed `lore branch merge start` failing with `Not a directory` where one branch replaced a file with a folder and the other edited it. It now reports a conflict at that path, which resolves to either side.
- `lore-revision`: Fixed `lore branch switch` and `lore sync` failing with `Not a directory` where they replace a file with a folder, or a folder with a file.
- `lore-revision`: Fixed `lore branch merge start` failing where both branches added the same path, one as a file and the other as a folder. It now reports a conflict at that path, which resolves to either side.
- `lore-revision`: Fixed `lore branch merge` failing when the merged branch removes a link whose repository has changes to merge, with `Link not found`, with or without a view, and when it renames in case a directory holding a link, with `file not found`. The merge carries the removal and leaves out the merge of the link's repository: it doesn't merge that repository, except for a link removed inside an out-of-view subtree the merge adopts whole, whose repository it merges and then leaves out. A `--link-message` naming a removed link is left out of the merge's commit, and `lore branch merge abort` puts a removed link back. A link the merged branch renames in case, or that stands below a directory it renames in case, is merged where the rename puts it. A directory the merged branch moves elsewhere removes the link inside it and adds it at the merged branch's pin, without the current branch's changes to its repository.
- `lore-revision`: Fixed `lore branch merge`, `lore sync`, `lore revision cherry-pick` and `lore revision revert` keeping a file the incoming side deleted, where a view excludes the file and the current branch changed it. They take the incoming deletion.
- `lore-revision`: Fixed `lore commit` committing a file in conflict that was deleted and staged for deletion, alone or with its directory, before the conflict was resolved. The commit refuses it as an unresolved conflict.
- `lore-revision`: Fixed `lore commit` failing with `file not found` after `lore branch merge resolve`, `lore revision cherry-pick resolve` or `lore revision revert resolve` of a conflict whose file the working tree lacks, such as a file the current branch deleted and the incoming branch changed. `lore status` lists a file the current branch deleted as a deletion in conflict rather than an add, and `lore commit` refuses it until it's resolved. A plain resolve commits the file the working tree holds at the path, or its deletion where it holds none, and no copy the merge wrote beside the file is left once the merge commits. Outside a linked repository, no empty directory is left where the current branch deleted the file's directory; one that holds other files stays.
- `lore-revision`: Fixed `lore revision info --delta` of a merge revision listing a deletion, at the path or at another path, where the current branch deleted a file or a directory, the incoming branch changed a file there, and the merge kept the deletion. The merge revision records no change there. Fixed it too for a `lore sync` of diverged history, which listed a deletion the remote made, read at a file the merge holds, and could name a deletion the merge made after another file.
- `lore-revision`: Fixed `lore revision info --delta` of a revision `lore commit --force` committed from changes staged on an earlier revision, or `lore commit --link` committed through one mount of a linked repository after `--link` commits through another mount moved its branch on, listing the deletion of a file the revision before it added, and such a commit giving a file it kept the history of that add.
- `lore-revision`: Fixed `lore branch merge` under a view filter, and a `lore sync` that merges a diverged branch under one, corrupting the merge where the merged branch pinned a link to another revision inside a subtree the view leaves out. Files of the merging repository took the content of files in the linked repository, and pushing the merge failed with `Address not found`. The link's pin now merges as in a merge with no view.
- `lore-revision`: Fixed `lore branch merge` under a view filter, and a `lore sync` that merges a diverged branch under one, losing a folder, file or link the merged branch put in place of one of another kind inside a subtree the view leaves out: a folder in place of a file or a link, a file in place of a folder, or a link in place of a folder. The merge now holds the path as the merged branch does.
- `lore-revision`: Fixed `lore branch merge`, `lore push --fast-forward-merge`, `lore revision cherry-pick`, `lore revision revert` and `lore sync` losing a change that only gives a file the executable bit or takes it away. The merge keeps it beside the other side's edit of the file or of other files, its move, a rename that only changes case, and the text merge of edits on both sides, and never reports a conflict for the bit. Where the other side deletes the file, puts another file in its place or replaces a directory above it with a link, that change wins. A file added at a path after a revision deleted the file there is a new file, and takes its own mode.
- `lore-revision`: Fixed the same commands reporting a conflict where one side moves a file and changes its executable bit and the other edits the file.
- `lore-revision`: Fixed the same commands losing a change that only mounts another directory of the revision a link pins, where the other side leaves the link as it was. Where both sides change the link and keep its pin, the diff takes neither change: `lore branch merge` and `lore sync` settle the two mounts in their link merge, and the other commands keep the link the side merged into holds.
- `lore-revision`: Fixed, on Linux and macOS, `lore branch merge resolve mine` and `theirs`, and the same resolves of `lore revision cherry-pick` and `lore revision revert`, giving a file the executable bit of the side resolved to. A resolve chooses the content where the two sides' content conflicts, and never undoes a change of the bit: every file it touches, conflicted or merged cleanly, keeps the bit the merge gives it.
- `lore-revision`: Fixed, on Windows, the same resolves committing the merge's executable bit for a file the side resolved to deleted and added again. The new file keeps its own bit.
- `lore-revision`: Fixed `lore sync`, `lore branch switch`, `lore branch merge`, `lore revision cherry-pick`, `lore revision revert` and `lore link update` giving a file the executable bit the incoming revision gives it, or taking it away, and then refusing for a local change to another file, a layer's sync included. They change no bit before every file has passed.
- `lore-revision`: Fixed `lore repository metadata set --binary` and `lore branch metadata set --binary` panicking the service with `Cannot request zero hash from store` when the file is empty, which dropped the client's connection
- `lore-revision`: Fixed `lore sync`, `lore branch switch`, `lore link update`, `lore branch merge`, `lore revision cherry-pick`, `lore revision revert` and the sync of a layer overwriting a local edit to a file the incoming revision moves and modifies, and carrying a local edit to a file it only moves to the new path. An edit at the path a move takes a file from refuses the move, as an edit at the path it moves to does.
- `lore-revision`: Fixed `lore stage move` of a file or directory out of a directory other than the root keeping it in both places: the next commit kept the directory it left as it was. A merge or restore that moved a file or directory out of a directory could do the same
- `lore-revision`: Fixed moves dropping changes already recorded on what they move. Moving a directory with `lore stage move` or `lore dirty move` no longer turns a child staged or marked for deletion into a moved one, and moving a directory over the directory that holds it with `lore stage move` keeps the deletions and additions staged below it. A file or directory added but not yet committed stays an addition when moved: `lore dirty move` recorded one a merge added as moved, and `lore stage move` did the same to one marked as added with `lore dirty`
- `lore-revision`: Fixed `lore stage move` leaving a directory unmarked when it moved a file or directory marked dirty into it and the directory was clean, so `lore status` and `lore stage` of a directory above it missed the moved changes
- `lore-revision`: Fixed `lore stage move` of a file or directory renamed in case alone, such as `Dir` to `dir`, reporting the file, or every file below the directory, as deleted and leaving each marked dirty
- `lore-revision`: Fixed `lore stage move` and `lore dirty move` of a directory into a directory below it never returning; they fail with an invalid argument error
- `lore-revision`: Fixed `lore commit` dropping the dirty mark of an unstaged change below a directory staged as moved or deleted, so `lore status` without `--scan` missed the change after the commit
- `lore-revision`: Fixed marking a change below a staged directory, with `lore dirty`, a scan or `lore stage`, dropping what was staged for that directory
- `lore-revision`: Fixed a scan limited to a path, such as `lore status --scan a/b/c`, that enters directories the tree lacks, leaving the directory above them unmarked, so `lore status` and `lore stage` of a directory above it without `--scan` missed the directories it added. Every name on the path is now checked before any is added, so a name the repository refuses adds none of the directories above it
- `lore-revision`: Fixed `lore unstage` of an added link clearing the change marks of every directory above it, which hid a file changed beside the link from `lore status` without `--scan`, and, once nothing below the root was marked, dropped the staged state holding it
- `lore-revision`: Fixed `lore stage --case rename` dropping the dirty mark of a file it takes back from a staged deletion and renames in case alone
- `lore-revision`: Fixed committing, `lore reset` and `lore dirty` leaving the nodes below an added directory they remove allocated in the saved state; they're now freed with it
- `lore-revision`: Fixed staging a file whose conflict was resolved turning it back into an unresolved conflict: a forced `lore stage`, `lore stage --case rename`, `lore stage move` or a staged deletion of the file kept the conflict and dropped its resolution and side. Staging a file again that was staged for deletion also keeps its merge state, where it cleared it
- `lore-revision`: Fixed `lore branch merge unresolve`, `lore revision cherry-pick unresolve` and `lore revision revert unresolve` of a file resolved with `mine` or `theirs` keeping the side the resolve took, so `lore status` still reported that side for the file in conflict again
- `lore-revision`: Fixed a directory being added again under a name the tree already holds when reading the tree failed part way. `lore status --scan`, `lore stage --scan` and `lore dirty` took a directory whose lookup couldn't be read for a missing one and added it a second time, leaving two entries of one name. They now stop with the error the read answered
- `lore`: Fixed two `lore_revision_tree_delete` calls on one handle, of a subtree and of a node below it, running at once, so the second staged less and reported a smaller count. They run one after the other, and the second rejects with `INVALID_ARGUMENTS` a node the first staged or detached. A call reports its events, a rejection included, once its batch is done, where it reported a rejection as it planned
- `lore`: Fixed `lore_revision_tree_delete` freeing the nodes of an added subtree at once, while an add or a move on the same handle could still reach them. The next `lore_revision_tree_commit` that goes ahead frees them, so adding again before that commit takes new nodes
- `lore`: Fixed `lore_revision_tree_move` calls on one handle, and a move beside a `lore_revision_tree_delete`, running at once: two calls moving nodes that shared a parent chain could leave a node in one directory's chain while its record named another, which failed the next `lore_revision_tree_commit`. They run one after the other, each checking its entries against the tree the one before left
- `lore`: Fixed a revision-tree handle committing many times over adds and deletes growing its tree's name tables until adding failed: once a commit freed a deleted node, no later commit dropped a name from that node's block while the handle kept the block in memory. Each commit now drops the names of freed nodes
- `lore`: Fixed `lore_revision_tree_list_children` failing when a child moved away while it listed the directory; it walks the directory again instead, so beside moves or deletions on the same handle it may report a child twice, which a caller drops by node ID, or leave out one moved or deleted meanwhile
- `lore`: Fixed the C API's revision-tree verbs reporting an entry the tree gave up on under concurrent changes as `LORE_ERROR_CODE_INTERNAL`; they report `LORE_ERROR_CODE_SLOW_DOWN`, which a caller can retry
- `lore-base`: Fixed `lore status --count` and the C API's revision-tree verbs reading the processor count they size their parallel work from on every call, on Linux from the cgroup files; they read it once per process
- `lore-storage`: Fixed `lore branch list` asking the remote about each branch name archived in the local store, and `lore_storage_mutable_list` listing keys that hold zero. The local mutable store kept an entry holding zero for a key stored or compare-and-swapped to zero, and created one for a compare-and-swap from zero to zero, where zero removes the key. It now removes the entry, and a list leaves out a key holding zero, an entry an older version wrote included.
- `lore-server`: Fixed Lore Server's DynamoDB mutable store keeping an item holding zero for a key compare-and-swapped to zero, and creating one for a compare-and-swap from zero to zero, as a branch created without revisions makes. It now deletes the item on the swap's condition, and writes none for a swap from zero to zero.
- `lore-server`: Fixed a DynamoDB compare-and-swap reporting success when DynamoDB refused it while returning an item that holds the expected value. It now fails with an internal error.
- `lore-revision`: Fixed `lore file obliterate` failing with `NotFound` for content the remote does not hold, such as content never pushed, after it had removed the local copy. The command now obliterates on the remote first and counts content the remote does not hold as obliterated there, so it removes the local copy and succeeds. A failed remote obliterate leaves the local copy in place.
- `lore-revision`: Fixed `lore file obliterate` on a path inside a link destroying the content of a file in the parent repository, locally and on the remote, removing the path from the working tree, and staging that file's deletion. It now fails with `NotSupported` and changes nothing.
- `lore-revision`: Fixed `lore reset --purge` deleting a committed file whose name the working tree spells in another case, such as `foo.txt` for a committed `Foo.txt`. On Windows and macOS that file is the committed file itself. The purge leaves it to the reset.
- `lore-revision`: Fixed `lore status` reporting the branch behind or diverged from the remote after a commit or sync killed before it stored the branch latest, then a push of the same revision killed once the remote had accepted it. The push run after it records the revision as the branch latest, so the branch reads as in sync with the remote.
- `lore-revision`: Fixed a read-only command going on without the metadata of a branch whose local copy of the metadata the instance cannot read, while the remote holds readable metadata. `lore branch diff` and `lore repository instance list` showed no name for such a branch. The command reads the remote's metadata.
- `lore-revision`: Fixed a file or a directory being deleted where the file system refused to rename it to another spelling of its name, as a sync, branch switch, merge, `lore reset` or `lore stage --case` does. On Windows and macOS the new spelling names the file or directory itself, which the recovery from the refused rename removed, with every file in a directory. The file or directory now stays under its old spelling. `lore reset` and `lore stage` report the refusal, and a sync, branch switch or merge warns once with how many moves it carried out without the refused rename, and the first of them.
- `lore-revision`: Fixed a `lore sync` or `lore branch switch` that failed to start updating the working tree leaving the repository held after the command completed. It logged "Repository has strong reference remaining after completion", and a build with debug assertions aborted.
- `lore-revision`: Fixed a view or an ignore file bringing back a path that a rule re-includes below a directory a rule after it excludes, where an earlier rule excluded the directory too. With `/**`, `!/a*`, `!*/d/y` and `/a/*`, `a/d/y` came back; it stays out, as it does without `/**`. An exclusion in `.lore/ignore` keeps such a path out too, as `/Game/Saved/` does for `Game/Saved/Config/game.ini` under `Saved/` and `!/Game/Saved/Config/`.
- `lore-server`: Fixed a server over a `remote` immutable store answering a fragment the remote lacks as a store failure and logging a warning for it. The fragment is answered as not found.
- `lore-revision`: Fixed `lore reset` leaving a path reported as changed after it restored a directory over a file added in its place, or a file over a directory, that was staged and then unstaged. The reset discards the add, so `lore status` and `lore status --scan` report no changes.
- `lore-revision`: Fixed `lore reset` of a file inside a link that the target revision does not hold, such as a file staged or marked dirty as an add, deleting the file from disk, discarding an unrelated dirty add in the parent, or refusing for the wrong reason. It fails with `Operation not supported` and leaves the file and its staged state as they were
- `lore-revision`: Fixed `lore reset --last-merged-from` of a file inside a link following the history of an unrelated parent file and resetting the file to the wrong revision. It fails with `Operation not supported`
- `lore-revision`: Fixed `lore reset` of a staged file inside a link, or of a directory inside a link holding one, discarding the staged edit. It refuses with `Failed to reset staged node`, as it does outside a link.
- `lore-revision`: Fixed resolving a folder that holds a link, such as the whole tree, failing with `NotSupported` at a link that holds conflicts. It now resolves the conflicts inside each link the merge merged
- `lore-revision`: Fixed `lore branch merge abort` failing where a linked repository holds a link of its own
- `lore-revision`: Fixed `lore branch merge start`, run again to merge the parent once the conflicts inside its links are resolved, failing with "File has local changes" where a conflict inside a link was marked resolved with the content on disk, and dropping that resolution from the merge
- `lore-revision`: Fixed `lore branch merge start`, run again while a conflict inside a link is unresolved, merging the parent anyway. It now stops and lists the conflicts left inside the links
- `lore-revision`: Fixed `lore branch merge resolve` inside a link mounted from a folder resolving a file outside the folder, or writing the chosen side to a path below the mount that the link does not hold
- `lore-revision`: Fixed `lore branch merge abort` restoring a conflicted file inside a link mounted from a folder with the content of the file at the same path outside the folder
- `lore-revision`: Fixed resolving a conflict between a link and a file or folder, or between a folder and a file inside a linked repository, committing a corrupt revision, failing with an internal error or resolving nothing. It now fails with `NotSupported`, naming the path and the command that aborts the merge, cherry-pick or revert.
- `lore-revision`: Fixed `lore branch merge start` merging the changes below a path where one branch added a link and the other a file or folder. It now reports a conflict at that path, as for a file and a folder.
- `lore-revision`: Fixed resolving a folder that holds a link, such as the whole tree, failing with an internal error. It now passes over a link without conflicts, and fails with `NotSupported` at a link that holds conflicts, which are resolved by naming the paths inside it.
- `lore-client`: Fixed a bare `lore branch merge resolve`, `lore revision cherry-pick resolve` or `lore revision revert resolve` printing "No conflicts resolved" before the error when the resolve is refused.
- `lore-server`: Fixed `ThinClientService.RevisionDiff` announcing no `DiffPartition` in a 3-way diff where one side re-pins a link to a revision with the same mounted content, for a caller who may read the linked repository. A caller who may not got one. Any link the diff adds, removes, re-pins or remounts now announces its partition to every caller, so a stream with no `DiffPartition` is the same for every caller.
- `lore-server`: Fixed `ThinClientService.RevisionDiff` returning no changes when `from` is an ancestor of `to` only through a merge's second parent, such as after a branch is merged back. When one revision is an ancestor of the other, the diff is now 2-way with no `signatureBase`, and lists every change from `from` to `to`. Previously, when `to` was the ancestor, the changes were reported in the wrong direction.
- `lore-server`: Fixed a revision tree request, through `ThinClientService.RevisionTree` or the earlier `RevisionTreeRequest`, leaving out a linked subtree, or reporting a tracking link as pinned, when the store refused a read as overloaded. The request now fails with `RESOURCE_EXHAUSTED`.
- `lore-revision`: Fixed `lore file diff <path>` at a link's mount path reporting every file under the mount as added, and `lore status --scan <path>` there reporting the mount as deleted with its files untracked. A diff or status of a path at or below a mount reports what the same command over the whole repository reports under that path, including for a link not yet committed, a mount missing from the working tree or holding a file, and links inside the linked repository.
- `lore-revision`: Fixed `lore status --scan <path>` for a new folder below a link mount changing the link in the staged tree, and for a new file there failing with a not-found error. Both report the new files as the same command over the whole repository does.
- `lore-revision`: Fixed `lore file diff <path>` and `lore status --scan <path>` below a link mount whose linked revision can't be read reporting every file under the path as added. They fail as the same command over the whole repository does.
- `lore-revision`: Fixed `lore file diff <path>` and `lore status --scan <path>` for a path the repository holds as a file and the working tree as a folder leaving out the file's delete. They report the file deleted and the folder added, as the same command over the whole repository does.
- `lore-server`: Fixed a server whose `[server.auth.oidc] token_exchange_issuer` names a separate token service refusing the partition-scoped tokens that service issues. The server finds the service's keys through its OIDC discovery document, and verifies a token naming the service as `iss` against those keys only, and every other token against the `jwt_issuer` keys only. The server now fetches the service's keys at startup and doesn't start if it can't, and `token_exchange_issuer` must be an http or https URL.
- `lore-server`: Fixed `LORE_MAX_THREADS` sizing the server's async threads as if it ran 2 network threads rather than one per processor, so the two together could exceed the limit.
- `lore-server`: Fixed the server counting its network threads from a processor count that can be lower than the one its async threads use, on Windows hosts with more than 64 processors. Both now use the same count.
- `lore-client`: Fixed `lore service start` failing with exit code 32 when no service executable is named. It starts the service from the `lore` that ran it, as the configuration reference describes. A command that the service carries out because `[service] use_automatically` is on still starts one only from a named executable
- `lore-revision`: Fixed `lore stage move` into a link placing the moved path at an unrelated node of the parent's tree, such as its root, and leaving the bytes untracked in the mount, where a later `lore reset --purge` deleted them. A move into, out of or within a link, and a move of the mount itself, are refused as not supported before anything is written, as is `lore dirty move` or `lore dirty copy` into a new directory inside a link. A `lore stage move`, `lore dirty move` or `lore dirty copy` onto the repository root is refused as invalid.
- `lore-revision`: Fixed `lore status` reporting a branch as diverged from the remote, with the remote ahead, after a `lore sync` or `lore commit` that committed a merge of the remote latest was killed while writing its result. The branch is reported ahead of the remote and not behind it, as it is after a merge commit that runs to its end.
- `lore-revision`: Fixed SWFS errors, and SWFS callbacks that fail, such as a read whose content cannot be found, going unlogged. They are now logged as errors as well as reported by `lore service status`
- SWFS: a service running commands for a caller with another global directory finds that caller's SWFS instances, where it reported `Repository not found`
- SWFS: `lore repository create` and `lore repository clone` refuse an SWFS instance once the service mounts eight, the most the SWFS driver attaches, with `Operation not supported: mounting another SWFS instance`. A ninth mount failed inside the SWFS library and left the client hung or disconnected. The count is the service's own mounts: the driver's limit is machine-wide
- SWFS: `lore repository create` and `lore repository clone` refuse a repository inside an SWFS instance's mount, and an SWFS instance inside another repository's working tree. Inside a mount they failed with `invalid path`
- `lore-revision`: Fixed a file missing from an SWFS instance after a sync brought it back. A sync that removed a file, such as `lore sync main@1 --force`, made SWFS keep the deletion as the user's own, so a later sync to a revision holding the file again left it absent.
- `lore-revision`: Fixed `lore status` reporting files as modified after `lore sync` or `lore branch switch` restored them, when `lore status --scan` had marked them as changed before. Local changes stay marked.
- `lore-revision`: Fixed `lore sync` marking a locally edited layer file as deleted, or a restored one as modified, including in a layer whose source path differs in case from the path its repository holds.
- `lore-revision`: Fixed `lore status` hashing every file a sync or switch wrote into a layer, by recording their modified times as it does for the files of the repository itself.
- `lore-revision`: Fixed `lore unstage` leaving a path replaced by one of the other kind, a directory over a file or a file over a directory, reported as `V entry -> entry`, after which `lore stage entry` committed both the file and the directory. Unstaging discards the added node, so the path returns to its never-staged state.
- `lore-revision`: Fixed `lore unstage` of a link's staged removal leaving the link staged for removal and dirty, though its files were back in the mount, where the link sits below a directory or anything else stays staged or dirty.
- `lore-revision`: Fixed `lore clone --vfs fuse` waiting for the root's ignore file from the remote before it started its background prefetch. The clone starts the prefetch first, and reads the ignore file while it caches the revision's state.

## v0.13.0 (Nov 5th 2026) [#2160]

### Breaking changes

- `lore.user.v1`: `PartitionList`, `PartitionAdd` and `PartitionRemove` are removed from `UserService`. `RepositoryRegistryService` replaces them
- `lore-server`: with `user_url` set, the server lists, registers and unregisters repositories through `lore.user.v1.RepositoryRegistryService` at `user_url` instead of the `UserService` partition RPCs. The directory at `user_url` must serve `RepositoryRegistryService` before the server is upgraded. Servers without `user_url` are not affected
- `lore-server`: with authorization configured, operations that change a partition check for the `write` permission. Any permission on a partition still allows reading it, but no longer allows writing. Servers without authorization configured allow every caller to read, write and administer repositories
- `lore-server`: changing branch protection, through branch protect and unprotect or by setting the `protect` key in branch metadata, needs `owner` or `admin` on the repository, or `write` for its creator. Previously anyone with access to the repository could set the branch protection rules
- `lore-server`: vending a presigned URL needs the `presign` permission on the repository instead of the `is_service_account` token claim. Servers without authorization configured still allow every caller to vend
- Server hooks: `Hook::pre_handler` is now `async`, so a pre-handler can call other services without blocking a worker thread. Custom hooks must make their `pre_handler` async and rebuild. A pre-handler is cancelled at its timeout (1 second, or the hook's own `pre_handler_timeout`), and a timeout now fails the request with `UNAVAILABLE` instead of `INTERNAL`
- A client connected to a `lores://` remote refuses to connect, failing with `NotSupported`, if the server advertises a storage, revision, lock, repository, or notification endpoint whose scheme doesn't authenticate the server, such as `lore://`. It used to connect to that endpoint without validating its certificate, and for gRPC without TLS. A server that advertises these endpoints must give them a scheme that authenticates the server, such as `lores://`
- Ignore and view rules match paths as `.gitignore` does. A character class or a `**` inside a name matches within one path component, so `!/Engine/Content**` re-includes `Engine/Content` but not its contents; use `!/Engine/Content`. A file with a re-inclusion that starts with a `**` component, such as `!/**/a/b`, fails to load
- `lore sync --reset` and `lore branch switch --reset` leave untracked files in place, and restore and delete only what the revision tracks, including committed files an ignore rule matches. `lore reset --purge` deletes untracked files
- `lore reset` does not consult the ignore file. It restores committed files an ignore rule matches, and `--purge` deletes untracked files whether or not an ignore rule matches them, as well as committed files the view excludes
- `lore-revision`: `.loreignore` applies only to paths the tree doesn't track yet. A committed file an ignore rule matches, such as one staged with `--force`, keeps flowing through like any other: `lore status --scan` and `lore status` report changes to it, `lore stage` and `lore dirty` pick them up without `--force`, and removing it, or the directory holding it, from the working tree stages its deletion. Previously only a change already staged was listed, and an unforced `lore stage` kept a removed file the ignore file names. New files the ignore file names are still left out, including a copy marked with `lore dirty copy` until it is staged, as is a directory that replaces a tracked file, or the reverse, where the rules exclude it. A file moved with `lore dirty move` stays tracked at its new path. See [ADR-00027](developing/decisions/00027-ignore-filter-applies-to-untracked-paths-only.md)
- `lore-revision`: a failed link push exits with its error's code rather than 255, and with the new code 52 (`LinkDivergent`) when the linked branch has newer revisions on the remote. Scripts should test a failed push for any non-zero code.
- `lore-revision`: Names Windows reads as something else are refused on every platform and skipped where a revision already holds one, so a clone or sync on Windows writes nothing outside the working tree: a drive (`C:`, `a:b`), a name ending in a dot, a space or a colon (`file.`, `...`), a stream type (`file::$DATA`), and the `.lore` directory's short name (`LORE~1`). Rename such a file or add it to `.loreignore`. Other names with a colon, such as `12:00.log`, are unaffected.

### Features

- When stdout is not a terminal, a command's progress bar prints a plain progress line every 30 seconds instead of drawing nothing. `LORE_PROGRESS_INTERVAL` sets the interval in seconds; 0 turns the lines off
- `lore.user.v1` gains `RepositoryRegistryService` (`RepositoryList`, `RepositoryRegister`, `RepositoryUnregister`), served at `user_url`.
- `lore-server`: forward the lock service's `Query` to a remote Lore server, opt-in via `lock_query` under `[server.grpc_public_services.forwarded_requests.enabled_rpcs]`. The peer serves it only when it has a lock store
- QUIC connections to replication peers configured under `[immutable_store.composite.replica_factory]` start with a 1 MiB initial congestion window. The same default applies to connections to the remote of a replicated store configured under `[immutable_store.replicated]` or as a composite tier in `replicated` mode. Set the window with `initial_cwnd_bytes`. Values below 2400 bytes are rejected at startup. A periodically refreshed connection starts with a window at least this large
- `lore-server`: connections accepted by `[server.quic_internal]` start sending responses with a 1 MiB congestion window. Set it per endpoint with `initial_cwnd_bytes`; `[server.quic]` keeps its congestion controller's default unless set. Values below 2400 bytes are rejected at startup
- The `AuthUserToken` event (C API: `lore_auth_user_token_event_data_t`) reports when the stored OIDC refresh token stops working, in `refresh_expires`, and whether one is stored, in `refreshable`. The deadline comes from the refresh token's lifetime in seconds in the token response: `refresh_token_expires_in` (GitHub, Azure AD, Google), else `refresh_expires_in` (Keycloak). If the provider returns a refresh token, but does not return a refresh token expiration time, the value is `0`, with `refreshable` set. `session_valid` is set while the token has not expired, or while a stored refresh token can still renew it. `lore_auth_local_user_info` and `lore auth info` emit both `AuthUserInfo` and `AuthUserToken` for an identity with a stored token. `AuthUserToken` carries the token only with `with_identity_token`, and an empty `token` without it.
- `lore auth login --reuse-session` (C API: `reuse_session` in `lore_auth_login_interactive_args_t`) returns the stored login when its session is still valid, as `session_valid` reports it, and starts no new login
- On Linux, the server's QUIC endpoints serve on several UDP sockets bound to their port, so clients spread across their receive buffers: `num_sockets` in `[server.quic]` and `[server.quic_internal]`, by default 16, capped at the processor count, and 4 for the internal endpoint. `num_listeners` is now optional and defaults to 1 per socket
- Server QUIC sockets ask for a 512 KiB receive buffer, as client sockets do
- `lore-server`: The `server.trust_unsigned_identity` setting lets a server without `[server.auth]` take the caller's identity from an unsigned bearer token on gRPC, HTTP and QUIC, so locks and notifications name the user instead of `<unknown>`. The identity grants no permissions. The server refuses to start with the setting beside `[server.auth]`.
- `lore`: Against a server that advertises no authentication, the client sends an unsigned token naming its identity when one is set, from `--identity` or the configuration.
- `lore-revision`: `[sync] prefetch_tree = true` in an instance's `config.toml` makes a sync fetch the target's tree blocks and name tables in bulk requests instead of one per block as the walk reaches it. It fetches the whole tree whatever the view, so it is off by default

### Fixes & Improvements

- `lore-server`: a storage copy reports a slow-down from the store as `SlowDown` over QUIC and `RESOURCE_EXHAUSTED` over gRPC, rather than as an internal error. The legacy `urc.rpc` gRPC get, put and copy streams report a slow-down as `RESOURCE_EXHAUSTED` as well
- When a storage copy receives a slow-down response, the client backs off and retries the copy, as it does an upload. If the server is still throttling when the retries run out, the client doesn't upload the payload. A write keeps it locally without marking it durable, as when an upload runs out of retries, and `lore push` fails with a slow-down error. The client used to upload the full payload straight away
- gRPC connections over TLS fall back to the bundled webpki roots when no certificates load from the OS trust store, as QUIC connections already did. They used to fail with `NativeCertsNotFound` before reaching the server
- `lore-server`: `[server.quic_internal]` sizes the flow-control windows of every connection it accepts for a 220 ms round trip, up from 100 ms. Replication connections to `OtherRegion` peers use the same value. A peer in a distant region can keep more than twice as much data in flight. Set the endpoint's value with `transport_rtt`
- `RelativePath` keeps its view offsets in 32 bits, 24 bytes inline rather than 40. On a 5-million-change sync this lowers the memory held after the diff from 2.9 to 2.7 GiB and the peak resident size from 3.9 to 3.8 GB
- `lore repository instance prune` keeps an instance whose instance file cannot be read on filesystems that report an empty directory's size as zero, such as Btrfs: reading a directory as a file fails instead of reading as empty
- `lore-server`: repository create and delete return `INVALID_ARGUMENT` for the all-zero repository id. This id identifies the namespace-root partition holding the repository name table
- `lore-revision`: links and layers name both sides of a mount the same way, `mount_path` and `source_path`. No behaviour change
- `lore-server` times each branch push in five phases: prepare, verify, fast-forward merge, commit and respond (`lore.branch_push.phase_duration`). A push dropped before it returns, such as by the handler timeout or a server shutting down, counts the phase it was in (`lore.branch_push.cancelled_phase`). Fragment verification also times its tree walk and existence checks (`lore.branch_push.verify_collect_duration`, `lore.branch_push.verify_exist_duration`). Each push also records its compare-and-swap attempts to store the new head (`lore.branch_push.store_latest_attempts`) and the size of its fast-forward merge (`lore.branch_push.fast_forward_merge_changes`), and logs the breakdown of a push that is cut off, fails or takes 10s or more at info
- A revision named by number, such as `<branch>@<number>`, resolves from the first page the remote returns for it, rather than walking the branch's history back from its latest revision one page at a time
- Following a branch's history through the remote no longer takes the revisions a page holds above the one it was asked for as that revision's ancestors. A page the server serves from a cached segment holds them, and finding the revision two branches share could answer with one of them, newer than the one they really share
- On Windows, a Lore service started while another serves the same socket exits without serving, as it does on Linux and macOS. It used to take the socket over and leave the first service running unreachable, holding its SWFS mounts until killed, so commands that each started a service at the same moment could leave one behind per extra command
- A branch push reads each mutable store key once, loads the branch alongside the pushed revision, and checks the branch name while it verifies the revision's fragments rather than before, so a push spends fewer store round trips before it moves the branch. A push the server refuses because the branch is protected no longer reinstates the name of a deleted branch first
- The deprecated `urc.rpc` `BranchPush` RPC treats a missing or deleted branch as `lore.revision.v1` `BranchPush` does: a branch without metadata is `NOT_FOUND` rather than an internal error, and a push to a deleted branch reinstates its name, or fails with `ALREADY_EXISTS` when another branch holds the name, rather than failing with `NOT_FOUND`
- `lore clone`, a whole-tree `lore diff` and `lore status --scan` skip the directories a view such as `*`, `!/*` excludes, and `lore clone` does not create them
- `lore sync --view` stores the view file as written, as `lore clone --view` does, so a rule escaped as `\!name` stays a literal name and `lore status --scan` does not report that file deleted
- Under a view re-including paths such as `!*/Saved/Config/`, `lore clone`, `lore sync`, `lore branch switch`, `lore layer add` and `lore reset` create a directory the view excludes only for content the view keeps below it, and `lore sync` and `lore branch switch` remove one they leave empty. A layer's mount directory and the parent directories of a dependency clone's targets stand whatever the view
- `lore stage --scan` and `lore status --scan` add a directory the view or the ignore file excludes, or delete one the view excludes, only along with content below it, also under rules such as `/Engine/` and `!/Engine/**/Build/`, and so does `lore stage --scan <directory>`. `lore --force stage --scan` adds such a directory whatever it holds
- The Lore service closes its SWFS mounts when it exits, however it is stopped. It used to leave them to EpicVFS, which kept a device for each one and could not be stopped afterwards: `sc stop EpicVFS` stayed in `STOP_PENDING` until the machine restarted
- `lore branch merge`, `lore branch diff`, `lore sync`, `lore revision cherry-pick`, `lore revision revert` and `lore push --fast-forward-merge` report a conflict for a file both sides change when its path holds glob characters such as `[` and `]`
- A layer's `lore sync --reset` and `lore branch switch --reset` discard local edits in the mount, as they do in the parent repository
- `lore reset --purge` deletes the untracked files in a layer's mount and leaves the layer's unchanged files in place
- `lore reset` keeps nested repositories, wherever they are in the working tree
- `lore stage --scan` leaves out a file an ignore file excludes where a re-inclusion reaches below a directory of the file's name, such as `Game/Saved/direct.log` under `Saved/` and `!*/Saved/*/Keep/`
- `lore stage` stages no deletion of the files a view leaves out of a sparse working tree, with or without `--force`, whether it scans, names a path or finds their directory removed from the working tree. Where a removed directory holds a staged add, it takes the add back. `lore file dirty` of a directory marks no deletion of the files the view leaves out
- `lore status` and `lore stage` report and stage the removal of a tracked file the working tree replaced with a directory an ignore rule excludes, such as `src/Intermediate` under `Intermediate/`, and leave out what the directory holds. `lore diff` of a path one revision holds a file at and the next a directory lists what the directory holds where the rules keep the directory and ignore a file of its name, such as under `Intermediate` and `!Intermediate/`
- `lore stage`, `lore diff`, `lore file dirty`, `lore lock` and `lore status` apply a directory-only ignore rule such as `Intermediate/` to directories only, so a file named `Intermediate` is tracked like any other file, and `lore unstage` applies a directory-only view rule to directories only
- `lore status` lists and `lore unstage` takes back staged changes to paths an ignore rule matches
- After a merge under a sparse view, diffs and file history list the files it adds, moves and deletes in out-of-view directories, and `lore link list` reflects the links it adds, moves and removes there
- `lore revision info --delta` lists a directory a merge brings in as added, with or without a view
- `lore clone`, `lore sync` and `lore stage --scan` skip a directory no re-inclusion can reach, such as `Game/Saved/Logs` under `Saved/`, `!*/Saved/Config/`
- A rename that changes only the case of a name, such as `Build.cs` to `build.cs`, commits without `--force`, and `lore sync`, `lore branch switch`, `lore branch merge` and `lore push --fast-forward-merge` apply it. This holds for renames committed with this release or later
- `lore branch merge`, `lore branch diff`, `lore sync`, `lore revision cherry-pick`, `lore revision revert` and `lore push --fast-forward-merge` keep both changes where one side moves a file, or a directory holding it, and the other edits the file, with or without the same move: the edit lands where the move put the file. Edits of a file on both sides conflict, whether or not either side moved it, as do a delete of a file against a move of it and two moves of a file to different names
- `lore sync`, `lore branch merge` and `lore link update` delete a directory the revision deletes, and `lore link remove` a link, when an unchanged file in it is spelled in another case than in the revision. The first three also delete a directory the revision deletes when the revision moves a file out of it
- `lore-aws`: concurrent requests for the same fragment share one `DynamoDB` read on the AWS immutable store. This covers association checks in put, copy, get and metadata lookups, and state reads in put and metadata lookups. A write by the same server to the association or the state ends the shared read, so a request to that server that starts after the write still sees it. A write by another server does not end it. `urc.store.immutable.aws.shared_reads` counts these reads by `read` (`exists` or `state`), `role` (`leader` for a request that read for itself, `follower` for one that shared a read in flight) and `context` (the operation)
- `lore-server` metrics: `BranchPush` records metadata blob reads for changed file metadata blocks during fragment verification. Under `lore.branch_push`, `metadata_reads` counts these reads per push, and `peak_metadata_reads_in_flight` records the maximum number of concurrent reads. The log line for slow, cancelled or failed pushes includes the same counts.
- `lore-revision`: a composite store read that misses locally goes to the durable store as soon as every read replica has missed, failed or been throttled, rather than after the full `[immutable_store.composite] durable_store_delay_ms`. The delay only holds back a durable read while a read replica is still working on it
- `lore-revision`: a file that `lore sync`, `lore branch switch`, `lore reset` or a merge writes into the working tree takes one stat after the write instead of two, unless it is stored as a fragment list on Linux or macOS. Its executable bit is set or cleared for user, group and others only where the user's bit differs from the one the revision holds, as `lore clone` and `lore file write` already decide it, so a file whose user bit matches keeps the group and other bits it has.
- `lore-revision`: `lore status --scan`, `lore stage --scan` and `lore sync --reset` are faster under a view or ignore file of about a thousand rules or more: each path is checked only against the rules that can apply to it, found by its leading directories. A view or ignore file holding thousands of paths in one directory also loads faster. Under a view of 6,342 rules over 450,000 files, a scan takes a seventh of the CPU time.
- `lore-revision`: a failed link push names the link (mount, repository and branch) and keeps the remote's reason.
- `glob-match`: a view or ignore rule whose fixed start or end differs from a path's, such as `/Game/**/*.uasset` against `Engine/Source/x.cpp`, fails without a full match. A query against a 100-rule ignore file takes a fifth of the time, and `lore status --scan` under a view of 6,342 rules over 450,000 files takes about 11 s instead of 157 s.
- `lore-revision`: collecting a diff's changes holds no spare capacity. On a 5-million-change sync this lowers the memory held after the diff from 3.9 to 3.0 GiB and the peak resident size from 4.9 to 4.6 GB
- `lore-revision`: a sync's verification keeps one list of changes and filters it in place. `LORE_MEMORY_STATS=1` reports the bytes the global allocator holds and their peak, and what the GrowVec and node block heaps hold, at exit, and logs them at debug level at each phase of a sync
- `lore-revision`: a sync that adds or deletes a whole directory walks it on every core. On a 5-million-entry delete the diff takes 13 s instead of 73 s, at the same memory
- `lore-server`: `lore.revision.v1.RevisionService/BranchList` loads up to 100 branches from the store at once instead of one after another, so listing a repository with hundreds of branches takes a fraction of the time. Branches are streamed in no particular order.
- `lore-server`: `[feature] branch_list_concurrency` sets how many branches a BranchList request loads at once. It defaults to 100 and must be at least 1.
- `lore-io`: on Windows, reading a directory queries the file system with a larger buffer, so a wide directory takes fewer queries, each of which every filesystem filter driver, such as antivirus, also sees. Every directory the library reads is read this way: the working-tree scans of `lore status --scan` and `lore stage --scan`, `lore stage` and `lore file dirty` on a directory, `lore reset --purge`, `lore link add`, directory renames in the working tree, and the local immutable store's index, among others. A directory is read as before when its path is relative, or when its search pattern, the path with `\*` appended, is longer than `MAX_PATH` (260 UTF-16 units with the terminator) without the `\\?\` prefix, or 512 units or longer with it.
- `lore-io`: on Windows, checking the type, size and modification time of a named path takes one query, which the file system answers without opening the file, where it took an open, a query and a close that every filesystem filter driver, such as antivirus, also saw. A filter driver that declines the query has the system open, query and close the file within that one call instead. Paths are checked this way for each dirty-marked or named file, as `lore status`, `lore stage`, `lore commit`, `lore reset`, `lore diff` and `lore sync` do; for each file `lore sync`, `lore branch switch`, `lore reset` and `lore branch merge` write or delete, and each move a rename can't make; for each file compared with stored content; for each directory a walk checks for a nested repository; and by `lore file write`, `lore file hash` and the local immutable and mutable stores. A symbolic link or other reparse point takes that query and then the open, query and close, except for a deletion, which checks the link itself with the query alone, and a path of `MAX_PATH` or more without the `\\?\` prefix is checked as before.
- `lore-revision`: a file that `lore sync`, `lore branch switch`, `lore reset` or a merge writes into the working tree no longer asks the file system to create the directory it goes in first. That cost every file a failed `mkdir` and a stat of a directory that was already there, and on Windows a `CreateDirectoryW` and an open, query and close of the directory that every filesystem filter driver also saw. The directory is now created only where the write finds it missing, such as below a directory the view excludes.
- `lore-aws`: Fixed `DynamoDB` batch reads resending unprocessed keys immediately and without limit while a table is throttled. Each batch of up to 100 keys now waits before resending its unprocessed keys. By default, waits start at about 50 ms and double up to about 1 s, and a batch gives up after 6 resends. A read that gives up fails rather than returning the items it got. The immutable store answers `SlowDown`, as it does when `DynamoDB` throttles a request outright. The lock store reports an internal error. `urc.aws.dynamodb.unprocessed_retries.exhausted` counts the reads that give up, labelled by `table_name` and `context`. `urc.aws.dynamodb.unprocessed_retries.total` gains buckets at 30, 50 and 100.
- `lore-server`: The AWS immutable store and lock store plugins take an optional `dynamodb_unprocessed_keys_retry` table to override these defaults. Its fields are `initial_backoff_ms`, `max_backoff_ms`, `max_attempts` and `jitter`. `max_attempts` counts resends.
- `lore-revision`: Fixed a removal that failed on Unix making what it named, or the directory a symbolic link there stood for, writable by everyone, while still failing. A removal takes a symbolic link as the link and does not follow it: `lore sync`, `lore branch switch`, `lore reset` and `lore branch merge` remove a symbolic link standing where a tracked directory was without changing the mode of the directory it stands for, though a file the revision tracks below the link is still removed through it, and they remove a symbolic link that names nothing instead of leaving it in place. On Unix, a failed removal of a file or an empty directory is not retried, since its own mode never refuses it, and a failed recursive removal is retried after adding only the owner's write permission to the directory itself, never through a symbolic link. On Windows, a failed removal is retried after clearing the read-only attribute of the name itself, a symbolic link's or junction's own rather than that of what it stands for.
- `lore-server`: Fixed replicated store and replica requests stalling while the replication client is refreshed or reconnected. A refresh waited for every request in flight and held back new ones until then, so one slow request could block the store until it finished. Requests in flight now finish on the old client, and new requests use the new client as soon as it connects.
- `lore-server`: Fixed delayed reconnection after a connection to a replication peer failed. Reconnection sometimes happened only after further requests failed. A request that started during a refresh could run on the new client while recording the old one, so its failure was ignored as already handled.
- `lore-revision`: Fixed `lore status --scan` and `lore stage` keeping an untracked entry an earlier scan recorded after an ignore rule excludes it
- `lore-revision`: Fixed `lore stage` and the replay of dirty marks after a sync, a branch switch or a commit taking in every file of a new directory where only some were marked, such as after `lore status --scan dir/a.txt` with `dir/b.txt` beside it. A new directory that is only marked is staged along with the files marked in it, and on its own only where it is empty. A directory `lore dirty --force` creates above a new file is untracked, as the file is, so `lore status` no longer lists it below a directory the ignore file names
- `lore-revision`: Fixed staging a file over a link leaving the link's registry entry behind, so a clone listed a link at a path holding a file. Staging the file now removes the link.
- `lore-revision`: Fixed a folder staged where `lore link remove` had removed a link failing with `Link not found` and being left out of the commit. The removal and the folder or file replacing it now commit together.
- `lore-revision`: Fixed `lore reset` of a committed folder or link failing with `File exists` or `Not a directory` where a file stands at its path. Reset of a link a file replaced now puts the link back.
- `lore-revision`: Fixed `lore unstage` of a staged link add or removal deleting or overwriting work at the mount. It refuses where the work is a local change to a file the link holds or a file standing at the mount, and points at `lore reset`. Files the link does not hold stay at the mount.
- `lore-revision`: Fixed `lore unstage` of a path whose type change is staged unstaging only the added node, and panicking where the replaced node was a link that was never committed.

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
- `lore`: `lore repository instance move` and `lore repository instance delete` open the repository without its stores
  or directory lock, so they can rename or delete the instance directory on Windows. A move records the new path only
  after the directory or mount has been moved, so a move that fails leaves the instance registered where it was, and a
  move whose registration cannot be written is undone. A move that cannot rename the directory says to move it manually
  and run `lore repository update-path` in it. A delete removes the registration first and then the files, and a delete
  that cannot remove them says which directory to delete manually. On Windows, run them from outside the instance,
  naming it with `--repository`: a working directory inside the instance fails the command with an explanation before
  anything is changed. When another Lore command or program has the instance's files open, a move or delete retries
  the rename or delete with backoff for about fifteen seconds before it fails
- `lore`: `lore repository instance move` on a SWFS instance unmounts the virtual filesystem from the source path and
  mounts it at the target, where it previously updated only the service's record of the mount path and left SWFS
  mounted at the source. If the target cannot be mounted, the instance is mounted at the source again

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
- `lore-server`: server-to-server QUIC traffic uses Cubic congestion control throughout. The internal QUIC server,
  the replicated immutable store's client and replication clients to peers in every region now use Cubic, where the
  server and cross-region clients used BBR. Cubic only shrinks the congestion window on packet loss, so a connection
  that sits quiet between bursts keeps its window rather than having it drained. The public QUIC server keeps BBR

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
- Fix re-pointing a link at another subtree, which failed with `Nothing staged for commit` and was undone by merges
  and branch switches. Both branches remounting the same link now stops the merge, as a divergent pin does. Links
  mounting a subtree change the hash of their parent directory from this release on
- Fix merges under a view filter dropping links inside an out-of-view subtree or failing with `Link not found`
- Fix `lore push` failing with `Failed to push link` after a merge brought a following link onto a branch the linked
  repository did not have yet
- Fix merges keeping the old repository of a link re-pointed at another repository at the same path
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
