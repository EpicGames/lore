---
lep: 2026-10-12-machine-level-lore-server-for-containers
title: A machine-level Lore server for containers
authors:
  - mattias.jansson
status: Accepted
created: 2026-10-12
updated: 2026-11-08
discussion: https://crowd.urc.internal.epicgames.net/Epic/Lore/change-request/1165
---

# A machine-level Lore server for containers

## Summary

Build machines run many containerized jobs that read the same repository content, and today each container's Lore client downloads that content from the remote and keeps its own copy. This proposal runs a Lore server on each build machine, on Linux, macOS or Windows, which the containers name in their remote URL. The machine server caches content on the machine's disk and writes it through to the upstream Lore server, which stays the durable store. Each container reaches it through a socket of its own, bind-mounted into the container. Storage requests travel on that socket in Lore's existing storage protocol messages, whole and answered out of order on one connection per process; every other request travels as gRPC on the same socket, which the machine server forwards unchanged to the upstream host it names. The machine server authorizes storage per partition with the container's ordinary Lore tokens, as the upstream would, and the upstream authorizes everything forwarded. Inside a container, Lore behaves as it does against the upstream.

## Motivation

Build and CI machines run concurrent jobs in containers, and the jobs on one machine read much of the same repository content. Each job's client fetches that content from the remote and keeps it in its own store, so the machine downloads and stores the same content once per container.

**Today's ways to share content do not reach across containers.**
- A shared store admits one process at a time: opening a local store takes exclusive locks, held while the store object lives, and a second process waits without a timeout ([`local/immutable_store.rs`](../../lore-storage/src/local/immutable_store.rs), [`lock.rs`](../../lore-base/src/fs/lock.rs)).
- The Lore service shares a store by running each relayed command as a task in its one process, against its own filesystem and under its own user, relaying only the caller's environment paths and working directory ([`connection.rs`](../../lore/src/remote/connection.rs), [`call.rs`](../../lore/src/remote/call.rs)). A machine-wide service would run every container's commands on the host filesystem, outside the containers.

**Jobs on one machine have different access.** A machine runs jobs for repositories with different access grants. A client store does not isolate partitions, because client stores open with the default settings ([`store/immutable.rs`](../../lore-revision/src/store/immutable.rs), [`local/immutable_store.rs`](../../lore-storage/src/local/immutable_store.rs)), so content shared between jobs needs an access check that a client store does not make.

**Reads are on every job's hot path.** Clone, sync and virtual filesystem reads all wait on fragment fetches, and a machine runs many such jobs at once.

**Build machines run every platform Lore supports.** Lore's clients and the Lore service run on Linux, macOS and Windows, and the service already relays over `AF_UNIX` sockets on Windows ([`local_socket.rs`](../../lore-transport/src/local_socket.rs)). A cache that serves one platform leaves the others downloading the same content once per container.

## Goals / Non-Goals

### Goals

1. **One content cache per machine.** The machine server serves content that one container reads or writes to the other containers from the machine's disk.
2. **Per-repository isolation.** A container reaches only the repositories its tokens grant, including content already on the machine's disk.
3. **Upstream semantics.** Pushes, branch and repository operations, locks, notifications and obliterates reach the upstream, which authorizes them for the caller and handles them as for any client.
4. **Container-side execution.** Repository logic, working trees and virtual filesystem mounts run in the container. The machine server opens no container file.
5. **Read throughput.** Reads of cached content over the socket sustain 10 Gbit/s of payload in aggregate across a machine's containers, measured as *Performance target* describes. The upstream, not the socket, bounds writes and cache misses.
6. **Standard clients and upstream.** Containers run ordinary Lore client builds of this release, and the upstream needs no change.
7. **Every client platform.** The machine server and its clients run on Linux, macOS and Windows.

### Non-Goals

- **Offline operation.** Writes, cache misses and forwarded requests need the upstream.
- **Per-user identity for content and mutable-key traffic.** The machine server fetches and writes content, and loads and stores mutable keys, at the upstream as its own identity (see *Machine server identity*).
- **New token issuance.** Containers carry the tokens Lore already issues; the machine server adds no issuer, key or token format.
- **Sharing one machine server across machines.** Each machine runs its own, with its own cache and identity.
- **User-visible changes inside containers.** Commands behave as they do against the upstream. The client changes this proposal needs are in its transport and environment handling, listed under Compatibility.

## Proposed Design

**Terms.**
- *Upstream*: the Lore server that holds the repositories durably, as the remote of every client today.
- *Machine server*: a `loreserver` on the build machine, which the containers name in their remote URL.
- *Client*: one process that talks to a remote: a `lore` command, an application through the C API, or the Lore service. Each opens its own connections.
- *Socket*: a stream socket at a path in the filesystem, one per container.
- *Repository* names the unit of access; its content lives in a store *partition* of the same id ([`types/mod.rs`](../../lore-base/src/types/mod.rs)).

```text
+------------------------------------------------------------------------------+
| Build machine                                                                |
|                                                                              |
| +--------------------------------------------------------------------------+ |
| | Container A                            Containers B, C, ... alike        | |
| |                                                                          | |
| |   ( lore CLI )   ( build tool, C API )   ( lore CLI )                    | |
| |         |                 |                    |                         | |
| |         +-----------------+--------------------+  relay                  | |
| |                           v                                              | |
| |              ( Lore service )---------------> virtual filesystem         | |
| |                 |            |           |        mounts                 | |
| |                 v            v           v                               | |
| |   [ instances:      ] [ shared store:   ] [ token store:     ]           | |
| |   [ working trees   ] [ metadata and    ] [ LORE_AUTH_PATH   ]           | |
| |   [ and .lore       ] [ state, mutable  ] [                  ]           | |
| |   [                 ] [ store, no file  ] [                  ]           | |
| |   [                 ] [ payloads        ] [                  ]           | |
| |                                                                          | |
| |   every Lore process runs with LORE_UDS_SOCKET naming the socket         | |
| +------------------------------------+-------------------------------------+ |
|                                      |                                       |
|                                      | [ container A's socket: storage     ] |
|                                      | [ protocol and gRPC, bind-mounted   ] |
|                                      v                                       |
| +------------------------------------+-------------------------------------+ |
| | ( loreserver: the machine Lore server, one socket per container )        | |
| |   storage: verifies tokens, authorizes each session per partition        | |
| |   composite immutable store:                                             | |
| |     [ local tier: disk cache, partitions isolated ]                      | |
| |     remote tier: reads misses from, writes through to the upstream       | |
| |   remote mutable store: mutable keys at the upstream, no branch state    | |
| |   gRPC: forwards each request to the host it names, token unchanged      | |
| |   [ machine identity credential ]                                        | |
| +------------------------------------+-------------------------------------+ |
+--------------------------------------|---------------------------------------+
                                       | upstream public endpoints
                                       v
  +----------------------------------------+  +------------------------------+
  | ( upstream Lore server )               |  | ( authentication service )   |
  |   [ durable immutable store ]          |  | ( notification service )     |
  |   [ mutable store: branches and keys ] |  |                              |
  +----------------------------------------+  +------------------------------+

  ( OIDC issuer ) <-- HTTP over the network, from containers that log in

  ( name ) a process     [ name ] a store
```

### Content: a composite store with the upstream as the durable store

The machine server's immutable store is a composite store: a `local` tier on the machine's disk and a durable tier in `remote` mode that reaches the upstream's public endpoint through the ordinary client protocol ([`settings.rs`](../../lore-server/src/settings.rs), [`server.rs`](../../lore-server/src/server.rs)).
- **Writes** go to the upstream first, and to the local tier once the upstream accepts them. A put that fully matches the local tier returns without reaching the upstream ([`composite.rs`](../../lore-revision/src/store/composite.rs)).
- **Reads** try the local tier first. A miss goes to the upstream under the same partition, and concurrent misses for one address share one fetch. A detached task writes the result to the local tier once the upstream answers, and a reader that arrives before that write ends takes the fetched result rather than fetching the address again ([`composite.rs`](../../lore-revision/src/store/composite.rs), [`inflight.rs`](../../lore-revision/src/util/inflight.rs)).
- **Capacity.** Eviction runs only when `max_capacity` is set ([`maintenance.rs`](../../lore-storage/src/maintenance.rs)), so the machine server's configuration sets `max_capacity` and `max_size` on its local tier. Neither has a default; the operator sizes `max_size` for the disk the local tier lives on, and `max_capacity` for the index memory the server can spare, since it keeps the whole index loaded (see *Memory*). Every entry is evictable, because the server builds its local store without protection for content not yet durable ([`server.rs`](../../lore-server/src/server.rs)).
- **Isolation.** The server's store isolates partitions ([`server.rs`](../../lore-server/src/server.rs)): it serves a read only from an entry that matches the requested partition, hash and context, and a query reports no other partition's copy ([`immutable_store.rs`](../../lore-storage/src/immutable_store.rs)). A put must carry its payload, which the server hashes against the address ([`put.rs`](../../lore-server/src/protocol/storage/put.rs)), and a copy from another partition checks the session's access to the source ([`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)).
- **Upstream sessions.** Each socket request reaches the upstream under the correlation id its session started with ([`get.rs`](../../lore-server/src/protocol/storage/get.rs)). A session holds its correlation id in the remote tiers until it stops or its connection closes, and while any session holds it, each remote tier keeps one upstream session pool per partition for that correlation id: one session on each upstream connection, which every pool shares ([`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs), [`remote.rs`](../../lore-revision/src/store/remote.rs), [`session.rs`](../../lore-transport/src/session.rs)). A container command therefore starts its upstream sessions once per partition in each tier, and commands that interleave on one partition keep each other's pools. Requests that no session's correlation id covers, such as those of the machine server's own gRPC endpoint, share each tier's most recent pool per partition.
- **Upstream connections.** The repositories a remote tier reaches share one set of storage connections to the upstream, `upstream_connections` of them, because the transport caches a connection by remote URL, identity, credential source and authentication mode, not by repository ([`connection.rs`](../../lore-transport/src/connection.rs)). The first use of a repository runs one connect, which every caller waiting on that repository shares, and the tier retries a failed connect with back-off ([`protocol.rs`](../../lore-revision/src/protocol.rs)). The remote tiers size their QUIC windows for 1 Gbit/s at a 100 ms round trip ([`storage_service/client.rs`](../../lore-transport/src/quic/storage_service/client.rs), [`client.rs`](../../lore-transport/src/quic/client.rs)). No setting sizes them from the upstream link yet, as the replicated store's `max_bandwidth_bytes_per_second` and `expected_rtt_ms` do for its connections ([`settings.rs`](../../lore-server/src/settings.rs)); that setting is follow-up work.
- **Not found.** An address neither tier holds reaches the client as not found: the remote tier answers an address the upstream lacks as not found, for reads of payloads and of metadata alike, and the server answers the client so ([`remote.rs`](../../lore-revision/src/store/remote.rs), [`get.rs`](../../lore-server/src/protocol/storage/get.rs)).
- **Upstream payloads.** The remote tier trusts the upstream by default: it checks a payload's sizes and neither decodes nor hashes it, unless `verify` is set ([`remote.rs`](../../lore-revision/src/store/remote.rs), [`read.rs`](../../lore-storage/src/read.rs)). A client checks each payload it fetches against its address, unless it trusts its remote: one run with `--trust-remote`, or a C API caller that sets `trust_remote` or opens a storage handle with `skip_verify`, checks only a payload's sizes ([`cli.rs`](../../lore-client/src/cli/cli.rs), [`open.rs`](../../lore/src/storage/open.rs), [`read.rs`](../../lore-storage/src/read.rs)). A client that finds a payload corrupt asks the machine server to heal it. The machine server then checks its local copy, drops it only if it is corrupt, and asks the upstream to heal its own copy only where the local copy was corrupt or missing, so the client's next read fetches a sound copy ([`verify.rs`](../../lore-server/src/protocol/storage/verify.rs)). A heal that arrives while the machine server writes a fetched copy of the address to its local tier waits for that write, so it checks the copy the write leaves ([`composite.rs`](../../lore-revision/src/store/composite.rs)). [ADR-00023](../developing/decisions/00023-data-verification-and-trust.md), as amended, records why the machine server trusts the upstream.

This addresses Goals 1, 2 and 6.

### Machine server identity

The remote tiers are the only paths that reach the upstream as the machine server: forwarded requests carry the caller's token.
- **Configuration.** A remote tier given no identity resolves one from the token store of the user the server runs as: the first identity stored there that the upstream accepts ([`connection.rs`](../../lore-transport/src/connection.rs)). The remote immutable store receives its `auth_url` setting as its identity, and the remote mutable store receives none ([`server.rs`](../../lore-server/src/server.rs), [`settings.rs`](../../lore-server/src/settings.rs)). So the token store of the machine server's user holds the machine identity's login alone. A setting that names the mutable store's identity is follow-up work.
- **Credential.** The identity is a machine identity at the upstream's provider, obtained with the client credentials grant where the provider supports it ([OIDC proposal, D3](2026-08-20-oidc-oauth2-authentication.md#d3-standard-grants-replace-the-custom-rpcs)) or a login stored for the server's user otherwise, readable only by that user. Each machine has its own identity.
- **Scope.** The remote tiers start their sessions per partition, each presenting a token scoped to that partition, for which they exchange the identity ([`remote.rs`](../../lore-revision/src/store/remote.rs), [`sessions.rs`](../../lore-transport/src/quic/storage_service/sessions.rs)). The identity's grants are the union of the repositories the machine's containers use, including repositories they create through the forwarded repository service.
- **Exposure.** A stolen copy of the credential reaches every repository in that union. That is the exposure the OIDC proposal keeps partition-scoped tokens to bound on build machines ([OIDC proposal, Tier 1 only](2026-08-20-oidc-oauth2-authentication.md#tier-1-only-drop-partition-scoped-tokens-entirely)); here it covers one machine's repositories, through a credential that only the machine server's user can read and that the provider rotates and revokes.

### Forwarded gRPC

The machine server serves no gRPC service of its own on the sockets. It forwards every gRPC request to the upstream host the request names ([`proxy.rs`](../../lore-server/src/uds/proxy.rs)).
- **Routing.** A client on the socket sends plaintext HTTP/2 whose authority is the host it would have dialed: the upstream, its authentication service or its notification service ([`grpc/mod.rs`](../../lore-transport/src/grpc/mod.rs), [`ucs_auth.rs`](../../lore-transport/src/auth/ucs_auth.rs)). `[server.uds.forward]` maps each such host, compared without regard to case, to the URL the machine server reaches it at. Each socket reaches each host over a channel of its own, with TLS for an `https` URL, so a container that stops reading a response holds up no other container's forwarded requests ([`proxy.rs`](../../lore-server/src/uds/proxy.rs)). It refuses a host the table does not name with `PERMISSION_DENIED`, and fails a request to a host it cannot reach with `UNAVAILABLE`.
- **Authorization.** A request's metadata, its token included, travels unchanged, so the upstream authorizes the caller and handles the request as for any client, including its push checks and hooks ([`branch_push.rs`](../../lore-server/src/grpc/handlers/branch_push.rs)). The forwarder verifies no token, checks no grant and keeps no response. Locks and pushes belong to the caller, as for a direct client.
- **Coverage.** Revision, repository, lock, notification, admin and environment requests, and the authentication service's, all pass through the same forwarder, so the machine server carries no code per service.
- **Existing forwarding.** A push runs on the server that receives it, against that server's own mutable store ([`branch_push.rs`](../../lore-server/src/grpc/handlers/branch_push.rs)). Locks live in the receiving server's memory in `local` mode ([`server.rs`](../../lore-server/src/server.rs)) or in a DynamoDB store through the AWS plugin ([`aws.rs`](../../lore-server/src/plugins/aws.rs)). The server forwards six requests, branch create, delete, get and list and repository create and get, to a peer's internal endpoint ([`forwarded_requests.rs`](../../lore-server/src/grpc/forwarded_requests.rs)); that path stays for its deployments, and the machine server does not use it.
- **Ordering.** A push's new fragments reach the upstream before the push does, because the composite store writes them through first. The upstream's push handler finds them when it verifies the fragments the push adds ([`branch_push.rs`](../../lore-server/src/grpc/handlers/branch_push.rs)).
- **Mutable keys.** The storage protocol's mutable operations (`MutableLoad`, `MutableStore`, `MutableCas`) and the key half of `GetResolved` and `PutResolved` go to the upstream through the machine server's `remote` mutable store, as the machine identity; the content half goes through the composite store ([`get_resolved.rs`](../../lore-server/src/protocol/storage/get_resolved.rs), [`put_resolved.rs`](../../lore-server/src/protocol/storage/put_resolved.rs)). The remote mutable store caches nothing, so each such operation costs an upstream round trip. It has no `list` ([`remote.rs`](../../lore-revision/src/store/remote.rs)); repository and branch listing go through the forwarder. The machine server keeps no branch state. As on any server, the generic write paths refuse the key types that have a dedicated request that validates the write, `RepositoryMetadata`, `BranchMetadata` and `BranchLatestPointer`, so a container writes `Untyped`, `BranchId`, `RepositoryId`, `Instance` and `Resolve` keys through the machine server, in the partitions its sessions are authorized for ([`mutable_store_handler.rs`](../../lore-server/src/protocol/storage/mutable_store_handler.rs)).
- **Health.** Health checks complete the storage handshake on a container's socket, which the server answers before any token ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)).

This addresses Goal 3.

### Obliterate

A client obliterates an address in its own store and then sends its remote's admin service an `Obliterate` request for the address in the repository's partition ([`file/obliterate.rs`](../../lore-revision/src/file/obliterate.rs), [`admin_client.rs`](../../lore-transport/src/grpc/admin_client.rs)). Through the machine server, that request is forwarded gRPC: the upstream checks the caller and runs its obliterate hooks and store obliterate as for any client, and its answer passes back unchanged.
- **The cache keeps its copy.** The storage protocol carries no obliterate, and the forwarder acts on no request ([`proxy.rs`](../../lore-server/src/uds/proxy.rs)), so the machine server's local tier keeps the address until eviction.
- **No event reaches it.** The machine server's notifications come from its own process or from a plugin, of which the tree registers none ([`server.rs`](../../lore-server/src/server.rs), [`registry.rs`](../../lore-server/src/plugins/registry.rs)).

Security Considerations describes the consequences. Follow-up work drops the cached copy once the upstream accepts a forwarded obliterate (see Unresolved Questions). This addresses Goal 3 for the upstream.

### Container authorization

Each container holds ordinary Lore tokens, provisioned for the repositories it may use, and the machine server validates its storage requests as the upstream does. This proposal adds no token issuance or handling.
- **Validation.** `[server.uds]` requires `[server.auth]`, which names the upstream's token issuer, audience and keys, and an authorizer that grants per partition: the authorization service's per-partition grants when `auth_url` is set, or the resource-grants authorizer for partition-scoped OIDC tokens ([`server.rs`](../../lore-server/src/server.rs), [`repository_authorizer.rs`](../../lore-server/src/authnz/repository_authorizer.rs)). The server refuses to start the sockets under the allow-all authorizer, which a missing `[server.auth]` selects, or under the global-grants authorizer, which lets every authenticated caller reach every partition ([`global_grants_authorizer.rs`](../../lore-server/src/authnz/global_grants_authorizer.rs)).
- **Environment.** A client reads its remote's environment through the forwarder, so a container sees the upstream's own authentication fields and service URLs, and logs in, exchanges and sends tokens as it would with the upstream ([`connection.rs`](../../lore-transport/src/connection.rs)).
- **Provisioning.** A container receives its tokens through existing mechanisms: a token store in the directory `LORE_AUTH_PATH` names, which the relay passes to the Lore service with each call ([`env.rs`](../../lore-base/src/env.rs), [`token_store.rs`](../../lore-credential/src/token_store.rs)), or the `--identity-token` and `--access-token` options ([`cli.rs`](../../lore-client/src/cli/cli.rs)).
  - Repository-service requests, which clone uses to resolve a repository name, carry the authentication token ([`grpc/mod.rs`](../../lore-transport/src/grpc/mod.rs), [`repository.rs`](../../lore-revision/src/repository.rs)), so a container that clones needs an identity token.
  - Exchanging that identity token for partition-scoped tokens calls the authentication service over gRPC ([`exchange.rs`](../../lore-transport/src/auth/exchange.rs), [`ucs_auth.rs`](../../lore-transport/src/auth/ucs_auth.rs)), which the socket carries. Only OIDC requests, which are HTTP, need the container's own network access ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs)).
- **Enforcement.** Storage on the socket runs the same storage service as the public QUIC endpoint ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs), [`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)): a session authorizes once at its start, is bound to one partition, and each command passes the same per-partition action check. The upstream alone authorizes forwarded gRPC. With the store's isolation, a container reaches exactly the partitions its tokens grant.
- **Client authorization.** The socket's storage client starts and authorizes its sessions as the QUIC client does ([`sessions.rs`](../../lore-transport/src/quic/storage_service/sessions.rs)): a session start presents the authorization the credentials exchange for, and when the server refuses an exchanged authorization, the client discards it and exchanges once more ([`exchange.rs`](../../lore-transport/src/auth/exchange.rs)).
- **Session lifetime.** As on QUIC, a storage session keeps the grants it took at its start until it stops or its connection closes ([`session.rs`](../../lore-server/src/protocol/storage/session.rs)).
- **Cost.** With `auth_url` set, each session start's grant check calls the authorization service ([`repository_authorizer.rs`](../../lore-server/src/authnz/repository_authorizer.rs)). The upstream authorizes each forwarded request once.

This addresses Goal 2.

### Socket transport

**Sockets.**
- **One per container.** `[server.uds] sockets` lists one socket path per container, each in a directory the machine bind-mounts into that container ([`settings.rs`](../../lore-server/src/settings.rs)). The sockets carry no TLS, because they never leave the machine.
- **Claim.** The server claims each path with a lock file beside it and refuses a path that another server has claimed or answers on. It removes whatever file is at the path, socket or not, binds a new socket at each start, and removes the socket file when it stops ([`local_socket.rs`](../../lore-transport/src/local_socket.rs), [`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). Startup fails if the server cannot claim or bind a socket, or register it with the runtime.
- **Platforms.** Linux and macOS use Unix domain sockets and Windows uses `AF_UNIX` sockets, through one implementation ([`local_socket.rs`](../../lore-transport/src/local_socket.rs)). The server creates a missing socket directory. On Unix it leaves the directory's permissions to the operator, and the socket file takes the permission bits `socket_mode` sets. On Windows a directory the server creates lets only SYSTEM, the Administrators and its owner change it, and users list and traverse it; the server refuses an existing directory that another account owns or may change. A `socket_mode` that lets others write, as the default `0o666` does, lets the machine's users connect to the socket there too; otherwise the socket keeps what its directory grants.
- **Two protocols.** A connection that opens with the storage magic `LOREUDS1` speaks the storage protocol; one that opens with the HTTP/2 client preface goes to the forwarder; the server closes any other ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). One timeout bounds the storage hello and the HTTP/2 preface, so a connection that stops partway through either gives back its place ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)).
- **Startup checks.** The server refuses to start with an empty `sockets` list, or with a `forward` key that carries a scheme or a port rather than a bare host name, and logs a warning when `forward` is empty, since its containers then reach no host over gRPC ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). It also refuses to start with `LORE_UDS_SOCKET` set in its own environment, which would send its own gRPC to the upstream into a socket ([`server.rs`](../../lore-server/src/server.rs), [`uds/mod.rs`](../../lore-server/src/uds/mod.rs)).
- **Shutdown.** At the shutdown signal the server stops accepting on its sockets and closes every socket connection, storage and forwarded alike, as it closes its QUIC connections ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). Clients reconnect with back-off once a server answers on the socket again (see *Reconnect*).

**Which socket a client uses.** `LORE_UDS_SOCKET` names the socket and turns it on for the whole process: every gRPC channel the process opens goes over the socket from the first, the authentication service's included ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs), [`grpc/mod.rs`](../../lore-transport/src/grpc/mod.rs), [`ucs_auth.rs`](../../lore-transport/src/auth/ucs_auth.rs)).
- There is no default socket path. A `lore-unix` remote without the variable fails with an error that names it.
- With the variable set, the process reaches over gRPC only the hosts the machine server forwards. A remote of another scheme also needs its host forwarded, and network access for its QUIC storage, over one storage connection ([`connection.rs`](../../lore-transport/src/connection.rs)). In a container with no network, as usual, only `lore-unix` remotes work.
- The relay passes only path variables to the Lore service ([`env.rs`](../../lore-base/src/env.rs)), and each process reads `LORE_UDS_SOCKET` from its own environment, so the container sets the variable for every Lore process, the Lore service's included.

**Storage protocol on the socket.**
- **Handshake.** The client sends a hello with its protocol version, whose flag and reserved bytes are zero. The server answers every client hello with its own, stating its version, its handler timeout and its limits, and then closes a connection of another version ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). The client refuses a server of another version without retrying ([`client.rs`](../../lore-transport/src/uds/client.rs)).
- **Messages.** The socket carries the `lore-storage/0.4` commands, each behind its 12-byte header with its command id and session id ([`command_header.rs`](../../lore-transport/src/quic/command_header.rs)), and none of QUIC's streams, packets or encryption. Neither side splits a message: no body exceeds one maximum-size fragment, its command overhead and the key `PutResolved` adds ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs)). Both sides check a header's size before reading its body, and the server closes a connection that sends a request with the error bit set ([`io.rs`](../../lore-transport/src/uds/io.rs)).
- **One connection per process.** A process holds one storage connection per socket, which every storage client and session in it shares ([`client.rs`](../../lore-transport/src/uds/client.rs), [`connection.rs`](../../lore-transport/src/connection.rs)). Session ids come from one counter across all of the server's socket connections, so an id from a closed connection names no session on a later one until the counter wraps; the server seeds the counter from the clock at each start, so ids after a restart begin elsewhere in its range ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs), [`session.rs`](../../lore-server/src/protocol/storage/session.rs)).
- **Out-of-order processing.** The server reads whole messages off the socket and starts handling each request as soon as it has read it, without waiting for the requests before it to finish ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). Responses go out in completion order, each written whole, and the client matches them by command id ([`io.rs`](../../lore-transport/src/uds/io.rs), [`client.rs`](../../lore-transport/src/uds/client.rs)).
- **Admission.** A connection has at most `credits` requests in handling. Request bodies, and for a read (`Get`, `GetResolved`) the largest response, count against `credit_bytes` per connection until the handler finishes, together with responses the socket has not taken yet. Large request bodies, reads and those unwritten responses also count against `socket_bytes` per socket ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). Past a limit the server stops reading, and the socket's own buffer holds the client back; the server never answers `SlowDown` for admission, where a QUIC request waits a bounded time for a permit and then answers `SlowDown` ([`settings.rs`](../../lore-server/src/settings.rs), `permit_timeout_ms`). The client learns `credits` from the server's hello and never has more requests outstanding ([`client.rs`](../../lore-transport/src/uds/client.rs)). A handler that runs past the handler timeout answers `SlowDown`, as on QUIC ([`quic/mod.rs`](../../lore-server/src/quic/mod.rs)).
- **Lost peers.** The server drops a peer that takes no response for 30 seconds. The client gives up on a connection where a request has waited past a deadline of at least twice the server's handler timeout while the server answered nothing ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs), [`client.rs`](../../lore-transport/src/uds/client.rs)).
- **Reconnect.** When a connection closes, the client opens a new one with the QUIC client's reconnect back-off, opens each session again on it, and sends a command the closed connection failed once more there. A compare-and-swap that the server applied before the close answers its resend with a conflict, as on QUIC ([`client.rs`](../../lore-transport/src/uds/client.rs)).
- **Known limitation.** Admission counts a request's body, not the read buffer the body arrived in, and the buffer stays in memory while any request whose body arrived in it is in handling, so the server can hold more memory than admission counts ([`io.rs`](../../lore-transport/src/uds/io.rs)). A small put body that a store keeps does not keep its read buffer in memory ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). The client's reader works the same way: a response body is a slice of the 1 MiB buffer it arrived in, so a caller that keeps a small body, as the virtual filesystem's fragment cache keeps the fragments it reuses, keeps the whole buffer in memory, beyond the byte budget the cache counts ([`io.rs`](../../lore-transport/src/uds/io.rs), [`fragment_cache.rs`](../../lore-revision/src/fs/fuse/fragment_cache.rs)). Follow-up work bounds both sides together.

**Remote URL.** A client names the machine server as `lore-unix://<upstream host>/<repository>` ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs)). The client keeps only the scheme, host and port of a remote URL and reads its path as the repository name ([`repository.rs`](../../lore-revision/src/repository.rs)).
- The host names the upstream that the machine server fronts. A client sends a stored token only to a remote whose host the token's domains cover, and accepts an exchanged token only if it covers that host ([`token_store.rs`](../../lore-credential/src/token_store.rs), [`exchange.rs`](../../lore-transport/src/auth/exchange.rs)), so a container's ordinary tokens work unchanged.
- The client names its shared store after the remote URL without its scheme ([`url.rs`](../../lore-revision/src/util/url.rs), [`shared_store.rs`](../../lore-revision/src/shared_store.rs)), so a container's store has the name it would have against the upstream when the `lore-unix` URL carries the upstream's host and port as the client would name the upstream.
- The client takes the service URLs its remote advertises ([`connection.rs`](../../lore-transport/src/connection.rs)). With the socket on, each becomes a channel over the socket whose authority is the advertised host, so `[server.uds.forward]` names every host the upstream advertises. Storage goes over the socket whatever storage URL the upstream advertises; the client uses that URL's host as the recipient its token exchange names ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs)).
- The client picks its notification client by the scheme of the notification URL, and no notification client registers `lore-unix` ([`notification.rs`](../../lore-revision/src/notification.rs), [`lib.rs`](../../lore-notification/src/lib.rs)), so a container receives notifications when the upstream advertises a notification URL.

**Listeners.** `[server.uds]` adds the sockets and changes none of the server's network listeners, which the operator configures as for any Lore server ([`server.rs`](../../lore-server/src/server.rs)).

This addresses Goals 2, 5, 6 and 7.

### Containers

Inside a container, the client names the machine server in its remote URL and keeps its own shared store and Lore service. A client caches fetched payloads only with `--cache`, for `clone --offline`, or for a virtual filesystem mount, and always caches state fragments ([`cli.rs`](../../lore-client/src/cli/cli.rs), [`repository.rs`](../../lore-revision/src/repository.rs), [`mount_resources.rs`](../../lore-revision/src/fs/swfs/mount_resources.rs), [`state.rs`](../../lore-revision/src/state.rs)). By default the machine server therefore holds the one copy of each file payload, and a container's store holds metadata and state. Virtual filesystem mounts run in the container's Lore service, as today.

This addresses Goal 4.

### Performance target

Goal 5 covers reads of content the machine server already holds. Writes wait for the upstream, and misses fetch from it, so the upstream link and the remote tiers bound them.
- **Benchmark.** Containers on one machine read through a warm machine server, with 1 and with 8 containers, at payload sizes of 16 KiB, 64 KiB and 256 KiB and a mix of the three. The `uds-bench` example replays read addresses through any remote and measures the socket's bare round trip ([`uds_bench.rs`](../../lore-server/examples/uds_bench.rs)).
- **Measures.** Aggregate payload throughput, server and client CPU per GB, and the p99 latency of reads under load.
- **Baselines.** A warm container-local store, and today's QUIC over loopback to the machine server.
- **Acceptance.** 10 Gbit/s aggregate at 64 KiB and 256 KiB payloads with 8 containers.
- **Fixed costs.** Each request costs the server an execution context ([`get.rs`](../../lore-server/src/protocol/storage/get.rs), [`util/mod.rs`](../../lore-server/src/util/mod.rs)), and each read costs the client a hash check unless its opener skips it ([`read.rs`](../../lore-storage/src/read.rs), [`open.rs`](../../lore/src/storage/open.rs)). The benchmark's CPU per GB at 16 KiB shows how far these fixed costs limit small fragments.

This addresses Goal 5.

## Compatibility

- **Wire format** —
  - The storage socket opens with a 16-byte client hello: the magic `LOREUDS1`, the version (`u16`, 1), flags (`u16`, 0) and reserved bytes (`u32`, 0). The server answers with a 24-byte hello: the magic, its version (`u16`), its handler timeout in seconds (`u16`), the largest message body (`u32`), `credits` (`u32`) and `credit_bytes` (`u32`). All fields are little-endian ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs)).
  - Messages follow as the storage protocol's 12-byte v4 header and body, unchanged. No body exceeds 262,480 bytes: a 256 KiB fragment payload, its command overhead, and 256 bytes for the key `PutResolved` adds. The client refuses a server whose hello states more ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs), [`storage_service/mod.rs`](../../lore-transport/src/quic/storage_service/mod.rs), [`client.rs`](../../lore-transport/src/uds/client.rs)). An error response carries its status in the size field and has no body ([`io.rs`](../../lore-transport/src/uds/io.rs)). QUIC framing is unchanged.
  - gRPC on the socket carries the existing gRPC messages unchanged, over plaintext HTTP/2.
- **Client/server protocols** —
  - No new RPCs, storage commands or authentication flows. A socket session starts with the same `Authorize` payload as a QUIC one ([`sessions.rs`](../../lore-transport/src/quic/storage_service/sessions.rs)).
  - The machine server is an ordinary client of the upstream's public protocols, and forwarded requests carry the caller's token. The upstream is unchanged.
  - A client of an earlier release fails on a `lore-unix` URL with "protocol lore-unix was not recognized" ([`connection.rs`](../../lore-transport/src/connection.rs)), and can still use the upstream directly. A client and server of different socket versions exchange hellos, after which the server closes the connection and the client reports both versions.
- **On-disk format** — N/A — the machine server's stores use existing formats, and container stores are unchanged.
- **CLI and public API** —
  - Remote URLs of the form `lore-unix://<upstream host>/<repository>`, for the CLI and for the C API, which takes remote URLs as strings.
  - Errors: a `lore-unix` remote without `LORE_UDS_SOCKET` fails with an error naming the variable ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs)); a missing socket fails as an unreachable remote ([`client.rs`](../../lore-transport/src/uds/client.rs)); a version mismatch names both versions; a host missing from `[server.uds.forward]` fails with `PERMISSION_DENIED` naming the host ([`proxy.rs`](../../lore-server/src/uds/proxy.rs)); an upstream outage fails forwarded requests with the upstream's status or `UNAVAILABLE`, and storage requests as store failures ([`get.rs`](../../lore-server/src/protocol/storage/get.rs)); a repository the machine identity lacks fails its writes and misses as store failures that name the repository.
- **Environment variables** — `LORE_UDS_SOCKET` names the socket and sends every gRPC channel of the process over it. Unset, the process behaves as today.
- **Client behavior** — With `LORE_UDS_SOCKET` set, a client dials the socket for every gRPC channel, in plaintext, keeping the advertised host as the authority; it holds one storage connection per socket, sized to the server's `credits`; and it reopens its storage sessions on a new connection after a close.
- **Server configuration** —
  - `[server.uds]` ([`settings.rs`](../../lore-server/src/settings.rs), [`server.rs`](../../lore-server/src/server.rs)): `enabled`; `sockets`, one path per container; `socket_mode`, the socket file's permission bits on Unix, whose others-write bit lets the machine's users connect on Windows (`0o666`); `credits` (4,000); `credit_bytes` (256 MiB); `socket_bytes` (1 GiB); `max_connections_per_socket` (64); `handler_timeout_seconds` (50, from 1 to 65,535); and `forward`, a table from host to URL. `[server.uds]` requires `[server.auth]` and a per-partition authorizer, at least one socket, and `forward` keys that are bare host names; an empty `forward` starts with a warning. Each socket gets a lock file beside it, `<socket>.lock`, which stays after the server stops ([`local_socket.rs`](../../lore-transport/src/local_socket.rs)). The server refuses to start with `LORE_UDS_SOCKET` set in its own environment.
  - `[server] upstream_connections` ([`settings.rs`](../../lore-server/src/settings.rs), [`remote.rs`](../../lore-revision/src/store/remote.rs), [`connection.rs`](../../lore-transport/src/connection.rs)): the storage connections the server's `remote` stores open to the upstream for each identity they authenticate there as (4, from 1 to 10, with 0 read as 1). The transport keeps connections per upstream and identity, not per endpoint or repository, so the repositories the server reaches at that upstream as one identity share them, and so does every handler, QUIC, gRPC and socket alike. The remote immutable store passes its `auth_url` setting as its identity, and the remote mutable store passes none (see *Machine server identity*).
  - `immutable_store.composite.local`: `max_capacity` and `max_size` set.
  - `immutable_store.composite.durable` in `remote` mode and `mutable_store` in `remote` mode, each with `remote.remote_url`, and the immutable tier with `remote.verify`, whether it checks each payload against its address (see below). No setting names the mutable tier's identity or the upstream link's bandwidth and round trip yet; both are follow-up work.
  - The existing `[server.grpc_public_services.forwarded_requests]` keeps its meaning, and the machine server does not use it.
- **Remote store verification (breaking)** — A `remote` immutable store, and a composite store's durable tier in `remote` mode, trust the upstream by default: they check a payload's sizes and neither decode nor hash it, where they checked every payload they fetched against its address before. `verify = true` in the store's `remote` table keeps that check ([`remote.rs`](../../lore-revision/src/store/remote.rs), [`settings.rs`](../../lore-server/src/settings.rs)). [ADR-00023](../developing/decisions/00023-data-verification-and-trust.md), as amended, records why a server that reaches its upstream this way counts as within that backend. Clients still check what they fetch from the server, unless they trust their remote.

## Non-Functional Considerations

- **Concurrency** — A process's requests run concurrently on its one connection and complete in any order. Concurrent misses for one address share one upstream fetch; puts do not coalesce. Pushes serialize at the upstream, as they do for direct clients.
- **Memory** — The server loads its whole store index when it opens the store ([`server.rs`](../../lore-server/src/server.rs)) and keeps it while it runs ([`local/immutable_store.rs`](../../lore-storage/src/local/immutable_store.rs)), so index memory follows the local tier's entry count, which `max_capacity` bounds. Admission bounds request bodies, reads in handling and unwritten responses by `credit_bytes` per connection, and large request bodies, reads in handling and unwritten responses by `socket_bytes` per socket. Three holdings escape it:
  - read buffers kept alive by the bodies they hold, on the server and on the client (see *Known limitation*);
  - cache fills, which run detached and hold their payload until the local tier has written it ([`composite.rs`](../../lore-revision/src/store/composite.rs));
  - the forwarder's HTTP/2 connections, each of which hyper bounds by its defaults: 200 concurrent streams, a send buffer of 400 KiB per stream, and receive windows of 1 MiB that grow with the measured bandwidth-delay product up to 16 MiB ([`proxy.rs`](../../lore-server/src/uds/proxy.rs), [hyper `h2/server.rs`](https://github.com/hyperium/hyper/blob/v1.12.0/src/proto/h2/server.rs), [hyper `h2/ping.rs`](https://github.com/hyperium/hyper/blob/v1.12.0/src/proto/h2/ping.rs)). `max_connections_per_socket` bounds how many a container holds. Settings for these limits are follow-up work.
- **Statelessness** — The machine server keeps no branch state; its local tier is a cache it can lose. Per-connection state is the session map, the admission counts and the requests in handling; per socket, a forwarding channel per host; per process, the socket session id counter.
- **Determinism** — Unchanged. Revisions and content are those of the upstream.
- **Throughput** — See *Performance target*. One reader per connection bounds a single process; the benchmark measures it.
- **Latency** — A cached read crosses one local socket. Misses, forwarded requests, mutable-key operations and the first use of a repository add the round trip to the upstream. Metadata reads have no priority on the socket, where the client sends a priority read as an ordinary `Get` ([`client.rs`](../../lore-transport/src/uds/client.rs)), and wait behind responses queued ahead of them.
- **Backpressure** — Past an admission limit the server stops reading the connection, the socket buffer fills, and the client's writes wait. The client never sends more requests than its `credits`.
- **Durability** — Content is durable once the upstream accepts it, which happens before the machine server answers a put, except for a put the local tier already matches.
- **Availability** — The machine server is a single point of failure for the containers on its machine. With the upstream down, reads of cached content continue in sessions already open, for callers whose tokens validate without the authorization service; session starts that need the authorization service, forwarded requests, mutable keys, writes and misses fail.
- **Fairness** — Each container has its own socket, so `max_connections_per_socket` bounds each container's connections and `socket_bytes` its large request bodies, reads in handling and unwritten responses; `credits` and `credit_bytes` bound each connection's share, small request bodies included. Requests with bodies under 4,096 bytes count per connection only, so a container may have up to `max_connections_per_socket` × `credits` of them in handling, 256,000 by default. Storage connections and forwarded gRPC connections share `max_connections_per_socket`: a process holds one storage connection, one gRPC connection for each Lore host it calls, and short-lived connections for the authentication service ([`grpc/mod.rs`](../../lore-transport/src/grpc/mod.rs), [`ucs_auth.rs`](../../lore-transport/src/auth/ucs_auth.rs)), and with the Lore service one process holds these for every command. The socket is the unit of fairness, and there is no limit per subject: containers on a build machine often run under one service account, so a limit per subject would pool unrelated containers.
- **Observability** — The server logs handler errors and timeouts on the socket exactly as on QUIC: internal errors at warn, user errors at info, and either at debug when not appropriate for logging ([`quic/mod.rs`](../../lore-server/src/quic/mod.rs)). The socket adds no metrics and no spans. The composite store adds two gauges, `local.caching_in_progress` and `local.caching_bytes`: the cache fills in progress and the bytes they hold ([`composite.rs`](../../lore-revision/src/store/composite.rs)). Retention follows the operator's log configuration.

## Migration Plan

The changes are additive except one default: a `remote` immutable store now trusts its upstream (see *Remote store verification* under Compatibility). Clients that name the upstream keep working.

| Client | Upstream URL | `lore-unix` URL, machine server of the same socket version | `lore-unix` URL, other socket version |
| --- | --- | --- | --- |
| Earlier release | Works as today | Protocol not recognized | Protocol not recognized |
| This release | Works as today | Served by the machine server | Refused at the handshake |

1. **Existing `remote` stores.** Before upgrading a server that already keeps an immutable store in `remote` mode, set `verify = true` in its `remote` table to keep checking each payload it fetches; leave it unset to trust the upstream. Rollback: set it back, which takes effect at the next start.
2. **Machine server.** Install a `loreserver` of this release on the build machine with its composite store, `[server.uds]` sockets and forwarding table, and `[server.auth]` configured, and a machine identity granted the repositories its containers use.
3. **Container images.** Build images with clients of this release.
4. **Trial.** One build machine switches first, as step 5 describes, and runs its usual jobs; step 6's rollback trigger applies to it. It measures what decides three follow-ups (see Unresolved Questions):
   - the cache fills in progress and the bytes they hold, through the composite store's `local.caching_in_progress` and `local.caching_bytes` gauges ([`composite.rs`](../../lore-revision/src/store/composite.rs));
   - the connections on each container's socket, counted from the host with `ss -x`, against `max_connections_per_socket`;
   - the upstream sessions the machine server holds open, which the upstream logs at debug as each starts and stops ([`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)), against the upstream's 10,000 sessions per connection.

   The other machines switch once the trial machine's job duration and failure rate hold at the baseline measured before its switch.
5. **Switch.** Provision each container with tokens for its repositories, bind-mount the directory of its socket, set `LORE_UDS_SOCKET` for every Lore process in it, and name the machine server in its remote URL. Instances cloned against the upstream clone again, because their configured remote names the upstream.
6. **Rollback.** Trigger: container job duration or failure rate above the baseline measured before the switch, or the machine server's logged store failures rising. Unset `LORE_UDS_SOCKET` and point the containers back at the upstream. Container stores hold no file payloads, so jobs fetch content from the upstream again; nothing is lost, because all content and branch state live on the upstream.

## Security Considerations

The trust model changes for content. Containers hold ordinary tokens scoped to their repositories, and the machine server validates and authorizes their storage requests as the upstream would. The upstream authorizes forwarded requests for the caller's own token, and trusts the machine identity for content and mutable-key traffic, granted the union of the repositories the machine's containers use.

- **Isolation between containers.** The machine server authorizes every storage session against the caller's token, with the storage service the QUIC endpoint runs, and its store serves a read only from the requested partition. A put needs the payload, so naming a hash grants nothing. The machine server refuses to run its sockets under the allow-all or global-grants authorizers.
- **Where tokens go.** For a network remote, the issuer decides which hosts receive its tokens through the domains it lists ([OIDC proposal, D5](2026-08-20-oidc-oauth2-authentication.md#d5-the-domain-check-keeps-its-logic-and-the-issuer-keeps-the-list)). A `lore-unix` remote names the upstream's host, so that check passes, and the process environment decides instead: the client sends every token, storage and gRPC alike, without TLS, to whatever listens at the path `LORE_UDS_SOCKET` names. Whoever sets a container's environment and mounts decides where its tokens go. On Windows the server refuses a socket directory that another account owns or may change, and creates a missing one that only SYSTEM, the Administrators and its owner may change; on Unix the directory the operator mounts decides who can bind a socket in it. Who can connect follows the directory and `socket_mode`, on Windows as on Unix ([`local_socket.rs`](../../lore-transport/src/local_socket.rs)).
- **The forwarder.** It reaches only the hosts `[server.uds.forward]` names, so a container cannot use it to reach any other host. It verifies no token and passes each one through to the upstream, which authorizes the caller.
- **The machine identity.** A stolen credential reaches every repository the machine's containers use (see *Machine server identity*). The upstream attributes content writes and mutable-key changes to the machine identity, not to the caller.
- **The sockets.** Any process that can reach a socket can connect, as the default `socket_mode`, `0o666`, allows on Unix and, through its others-write bit, on Windows ([`settings.rs`](../../lore-server/src/settings.rs), [`local_socket.rs`](../../lore-transport/src/local_socket.rs)), but every storage session needs a valid token, and the upstream checks every forwarded request. The storage handshake answers without a token.
- **Revocation and expiry.** The machine server authorizes a storage session once, at its start ([`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)), so the session keeps its grants until it stops or its connection closes, as on QUIC, and a process keeps its socket connection while it runs ([`client.rs`](../../lore-transport/src/uds/client.rs)). An expired or revoked token's grants last that long in sessions already open; checking expiry on each command is follow-up work (see Unresolved Questions).
- **Obliterate.** An obliterate, through the machine server or not, leaves the machine server's cached copy in place until eviction (see *Obliterate*). Reads try the local tier first ([`composite.rs`](../../lore-revision/src/store/composite.rs)), so that content stays readable to containers with access to its repository. A put of the same content matches the local tier and never reaches the upstream, so the upstream fails the next push that names it ([`composite.rs`](../../lore-revision/src/store/composite.rs), [`branch_push.rs`](../../lore-server/src/grpc/handlers/branch_push.rs)).
- **Untrusted input.** The storage socket checks a header's size against the largest message before reading the body, closes a connection whose framing is malformed, answers a request it cannot parse with `InvalidCommand`, and bounds the handshake with a timeout ([`io.rs`](../../lore-transport/src/uds/io.rs), [`uds/mod.rs`](../../lore-server/src/uds/mod.rs)). The server refuses connections past a socket's limit and drops a peer that takes no response ([`uds/mod.rs`](../../lore-server/src/uds/mod.rs)).

## Privacy Considerations

- The machine server's cache holds content of every repository its containers used, on the machine's disk, as each container's store does today for its own repositories.
- The machine server receives its containers' tokens: it verifies those of storage sessions and passes those of gRPC requests through.
- Its socket logs carry what the QUIC storage service's carry: the command headers of failed requests, the addresses of fragments not found, and the repository and correlation id of each session start at debug ([`quic/mod.rs`](../../lore-server/src/quic/mod.rs), [`get.rs`](../../lore-server/src/protocol/storage/get.rs), [`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)), kept as the operator's log configuration says.
- Content obliterated at the upstream stays in the cache until eviction.
- Container stores hold no file payloads by default, so removing a container leaves no shared content behind in it.

## Risks and Assumptions

**Assumptions**

- **Assumption:** One connection per process carries the share of the 10 Gbit/s that one process needs. — *invalidated if:* the benchmark shows one connection's reader capping a process below its need; clients would then open several connections.
- **Assumption:** The upstream's provider can grant each machine identity the union of its containers' repositories, and keep it in step as containers gain repositories. — *invalidated if:* policy requires the upstream to see each caller on content traffic too; the content tiers would then need the caller's token, which server-side store calls do not carry ([`util/mod.rs`](../../lore-server/src/util/mod.rs)).
- **Assumption:** The machine server reaches the issuer's keys, the authorization service and every host it forwards to. — *invalidated if:* they are reachable only from the upstream's network, which would need them copied or proxied to the machine.
- **Assumption:** Containers that log in through an OIDC issuer reach it over HTTP, or hold provisioned tokens. — *invalidated if:* neither holds, in which case containers cannot obtain tokens.
- **Assumption:** Reads of cached content dominate a job's store traffic. — *invalidated if:* jobs mostly miss the cache, so the upstream link, not the socket, bounds them.

**Risks**

- **Risk:** An upstream outage stops every container's writes, misses and forwarded requests. — *mitigation:* cached reads continue in open sessions; operators can point containers at the upstream when it returns, as in the rollback.
- **Risk:** Obliterated content stays readable from the cache, and re-added content never reaches the upstream. — *mitigation:* eviction, until the follow-up that drops the cached copy once the upstream accepts a forwarded obliterate; an obliterate sent to the upstream directly, or through another machine, still leaves the copy until eviction.
- **Risk:** The upstream refuses the machine identity: it lacks a grant a container's token has, for example for a repository the container just created, or its login has lapsed. — *mitigation:* provisioning grants the machine identity each container's repositories. A refused write or miss reaches the container as a store failure, the storage protocol's `Failed` status, since the container can do nothing about the refusal, and answering `NotAuthorized` would tell the client its own token was refused ([`get.rs`](../../lore-server/src/protocol/storage/get.rs), [`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)). The machine server logs the upstream's refusal with the address. It keeps no connection the upstream refused, so granting the identity or logging it in again takes effect without a restart ([`protocol.rs`](../../lore-revision/src/protocol.rs)).
- **Risk:** The upstream advertises a host that `[server.uds.forward]` does not name. — *mitigation:* the forwarder refuses it with `PERMISSION_DENIED` naming the host.
- **Risk:** The machine server fails. — *mitigation:* the service manager restarts it, and clients reconnect with the QUIC back-off and resend what the closed connection failed; operators can point containers at the upstream.
- **Risk:** Per-request fixed costs limit small fragments. — *mitigation:* the benchmark measures CPU per GB at 16 KiB; a batched get is the follow-up if they do.
- **Risk:** A miss storm spawns detached cache fills that hold payloads outside admission. — *mitigation:* the upstream link bounds how fast fills arrive; the trial (Migration Plan, step 4) measures the fills in progress and the bytes they hold, and fills get a bound if those show pressure.
- **Risk:** Held upstream pools reach the upstream's session limit. Each correlation id a container's sessions hold keeps, in each remote tier, one upstream session per partition on every upstream connection, and the upstream refuses a session start past 10,000 sessions on one connection ([`remote.rs`](../../lore-revision/src/store/remote.rs), [`session.rs`](../../lore-server/src/protocol/storage/session.rs)). Many containers running commands at once over many repositories approach it, and past it their misses and writes fail as store failures. — *mitigation:* the trial counts the sessions the machine server holds open at the upstream, and the remote tiers get a cap on held correlation ids, past which calls share the most recent pool, if those approach the limit.
- **Risk:** A request in handling keeps alive the read buffer that holds its body. — *mitigation:* the handler timeout bounds how long, and the server copies small put bodies out; accepted.
- **Risk:** One container takes most of the server's capacity. — *mitigation:* per-socket connection and byte limits, and per-connection credits; the socket is the unit of fairness (see *Fairness*).
- **Risk:** Virtual filesystem mounts cache payloads in each container, which duplicates the machine server's copy. — *mitigation:* follow-up work has a mount with a `lore-unix` remote skip payload caching, read file content through the socket, and warm the machine store instead.

## Drawbacks

- The upstream attributes content and mutable-key traffic to one identity per machine.
- The machine server authorizes a storage session once, at its start, so a session keeps reading and writing after its token expires or its grant is revoked, until its connection closes, as a QUIC session does ([`storage_service_v4.rs`](../../lore-server/src/quic/storage_service_v4.rs)).
- Every container read crosses a socket, where a payload cached in the container would not.
- Forwarded requests add a hop to every branch, repository, lock and authentication request.
- Each socket carries two protocols, so the machine server runs an HTTP/2 server beside its storage reader on every container's socket.
- A client trusts its environment and the filesystem, not TLS and the issuer's domain list, to decide where its tokens go.
- Metadata reads lose the priority the QUIC client gives them on streams of their own ([`client.rs`](../../lore-transport/src/quic/client.rs), [`nametable.rs`](../../lore-revision/src/nametable.rs)).
- A process with `LORE_UDS_SOCKET` set sends all its gRPC to the socket, so it reaches only the hosts the machine server forwards to.

## Alternatives Considered

### Status quo: a store per container

*Rejected because:* each container downloads and stores the same content, so the machine holds one copy per container.

### The machine server at a network address over today's QUIC and gRPC

Containers would reach the machine server over the machine's network, with today's transports and URL schemes and a host name under the deployment's domain.

*Rejected because:* QUIC always runs TLS ([`server.rs`](../../lore-server/src/server.rs)), so both ends encrypt and decrypt every byte on the same machine, through userspace UDP, at the throughput Goal 5 sets; and every container needs a network route to the host and a certificate it trusts. The benchmark measures QUIC over loopback as a baseline.

### A served shared store

A store server would hold one shared store and serve the store traits over a local socket, and the administrator would register each container's repositories.

*Rejected because:* registration cannot confine content. A put without a payload links the payload any partition holds for the hash ([`local/immutable_store.rs`](../../lore-storage/src/local/immutable_store.rs)), so a container that knows a hash reads the content. The machine server inherits the rules that make partition grants hold: a put carries its payload ([`put.rs`](../../lore-server/src/protocol/storage/put.rs)) and a copy checks its source ([`copy.rs`](../../lore-server/src/protocol/storage/copy.rs)).

### One machine service that runs every container's commands

One Lore service for the whole machine would hold the store and run every container's commands.

*Rejected because:* it would run each container's commands as tasks in one host process, against the host filesystem and outside the container's resource limits, with every path translated between container and host ([`connection.rs`](../../lore/src/remote/connection.rs), [`call.rs`](../../lore/src/remote/call.rs)).

### gRPC for storage on the socket

*Rejected because:* gRPC carries each fragment as a protobuf message with its payload in a `bytes` field ([`storage.proto`](../../lore-proto/proto/lore/storage/v1/storage.proto)), in HTTP/2 frames of 16 KiB, with receive windows of 2 MiB per stream on the client for reads and 1 MiB on the server for writes by default, which Lore does not change ([hyper `h2/client.rs`](https://github.com/hyperium/hyper/blob/v1.9.0/src/proto/h2/client.rs), [hyper `h2/server.rs`](https://github.com/hyperium/hyper/blob/v1.9.0/src/proto/h2/server.rs)). The storage protocol carries a 12-byte header and the raw payload ([`command_header.rs`](../../lore-transport/src/quic/command_header.rs)).

### A gRPC router on the machine server

The machine server would serve the environment itself, check partition access before forwarding, and intercept obliterates to drop its cached copy first.

*Rejected because:* the upstream authorizes each forwarded request for the caller's token, so a second check costs an authorization call per request when `auth_url` is set ([`repository_authorizer.rs`](../../lore-server/src/authnz/repository_authorizer.rs)) without narrowing what a caller reaches; and forwarding by host carries the authentication and notification services with no code per service ([`proxy.rs`](../../lore-server/src/uds/proxy.rs)). Its one gain, dropping an obliterated address from the cache, needs no router: follow-up work has the forwarder decode the admin service's `Obliterate` request alone and drop the address once the upstream accepts it.

### One socket directory shared by every container

Every container would mount one directory holding a storage socket and a gRPC socket.

*Rejected because:* a socket per container lets the server bound each container's connections and buffered bytes (`max_connections_per_socket`, `socket_bytes`), and a container that mounts only its own directory cannot reach another container's socket.

### One forwarding connection per host for every container

The machine server would reach each forwarded host over one HTTP/2 connection that every container's requests share.

*Rejected because:* a container that stops reading a response fills the shared connection's flow-control window, which stalls every container's forwarded requests to that host until the stream ends. A connection per container and host confines the stall to the container that caused it, at the cost of one upstream connection per container and host, still no more than direct clients open, which hold one per process.

### Several socket connections per process

Each process would open several connections, as the QUIC client spreads its requests over streams.

*Rejected because:* each extra connection would cost its own session starts and credit window, which is why every storage client of a process shares one ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs), [`connection.rs`](../../lore-transport/src/connection.rs)), and requests on one connection already complete out of order. The benchmark decides whether a process needs more.

### Splitting messages into interleaved chunks

*Rejected because:* no message body exceeds 262,480 bytes: a 256 KiB fragment payload, its command overhead, and 256 bytes for the key `PutResolved` adds ([`uds/mod.rs`](../../lore-transport/src/uds/mod.rs), [`storage_service/mod.rs`](../../lore-transport/src/quic/storage_service/mod.rs), [`types/mod.rs`](../../lore-base/src/types/mod.rs)). Interleaving would therefore save at most the time to send one message, about 2.1 Mbit, or 0.2 ms at 10 Gbit/s, and would cost a reassembly copy on both sides.

### Machine-issued container tokens

A command on the machine would sign tokens for each container with a local key, and the machine server would verify them against a local key set.

*Rejected because:* it adds an issuer, a key to protect and a token format, where containers can carry the tokens Lore already issues and the upstream already accepts, which also lets forwarded requests carry the caller's identity.

### Container identity from the socket

The machine server would authorize by which socket a request arrives on, or by the connecting peer's credentials, in place of a token.

*Rejected because:* the authorizer decides from a token and a partition ([`repository_authorizer.rs`](../../lore-server/src/authnz/repository_authorizer.rs)), so it would need a new identity path through every service, and the upstream could not authorize forwarded requests for the caller.

### The replicated store as the durable tier

The durable tier would use the upstream's internal replication endpoint.

*Rejected because:* that endpoint gives blanket access to every partition behind a client certificate ([`server.rs`](../../lore-server/src/server.rs)), where the public endpoint authorizes the machine identity per partition.

### The existing request forwarding

The machine server would forward requests through the forwarding the server has today.

*Rejected because:* it covers six requests and not push ([`forwarded_requests.rs`](../../lore-server/src/grpc/forwarded_requests.rs)), and it targets the upstream's internal endpoint, whose forwarded branch handlers act on the forwarded user id without a check of their own ([`branch_create.rs`](../../lore-server/src/grpc/forwarded_revision/v1/branch_create.rs)).

## Prior Art

- **Perforce Proxy.** A proxy near the users caches file revisions fetched from the central server and serves later requests for them from its cache. It holds no metadata, and its cache accumulates until an administrator removes files ([P4 Proxy](https://help.perforce.com/helix-core/server-apps/p4sag/current/Content/P4SAG/chapter.proxy.html)). The machine server splits the same way: it caches content, and everything else goes to the upstream.
- **Nix.** Clients send store operations to one daemon per machine over a Unix socket ([Nix manual, Multi-user mode](https://nix.dev/manual/nix/stable/installation/multi-user.html)), as containers reach the machine server here.

## Unresolved Questions

None remain. Follow-up work:

- Drop an address from the local tier once the upstream accepts a forwarded `Obliterate` for it; the forwarder decodes that one request.
- Bound fills if the trial (Migration Plan, step 4) shows the fills in progress or the bytes they hold pressing on memory.
- Give forwarded gRPC connections a limit of their own if the connections the trial counts per socket show them competing with storage connections for slots.
- Cap the correlation ids the remote tiers hold pools for if the upstream sessions the trial counts approach the upstream's limit.
- Add a batched get if the benchmark's CPU per GB at 16 KiB shows per-request costs limiting small fragments.
- Bound the memory a socket read buffer keeps alive for the bodies sliced from it, on the server and on the client.
- Make the forwarder's HTTP/2 stream limit, send buffer and windows settings of their own.
- Add a setting that names the identity of the `remote` mutable store.
- Add settings for the upstream link's bandwidth and round trip, which size the remote tiers' QUIC windows.
- Have a virtual filesystem mount with a `lore-unix` remote skip payload caching, read file content through the socket, and warm the machine store, with fetch deadlines sized for the socket.
- Check a storage session's token expiry on each command, on the socket and on QUIC alike.
