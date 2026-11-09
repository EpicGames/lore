# Run a machine server for containers

A [machine server](../glossary.md#machine-server) is a `loreserver` on a build machine that the machine's containers use as their remote. It keeps one copy of the content they read on the machine's disk, writes what they push through to the [upstream](../glossary.md#upstream), and forwards every other request to the upstream unchanged. Each container reaches it through a socket of its own, bind-mounted into the container, so a container needs no network access to the upstream. Use this guide to set one up for a machine that runs several containers against the same upstream. It assumes the upstream runs and that the containers' users already have accounts there.

The examples use Linux paths and Docker. The machine server and its clients work the same on macOS and Windows, with paths of those platforms.

## Prerequisites

- `loreserver` and `lore` of the same release on the build machine, and `lore` of that release in each container image. See [Deploy a local Lore Server](deploy-local-lore-server.md) and [Install the Lore CLI](install-lore-cli.md).
- A user account on the machine for the server to run as.
- A machine identity at the upstream's identity provider, granted every repository the machine's containers use, with an API key or client credentials for it. The server fetches and writes content at the upstream as this identity.
- The upstream's settings: its URL, its authentication service, the issuer and audience of its tokens, and every service host it advertises to clients.

## Steps

1. **Log the machine identity in for the server's user.**

    Run this as the user the server runs as, with the environment the server will run with. On a machine without a keyring service, `LORE_AUTH_STORE=fallback` keeps the token store and its key in files under `LORE_AUTH_PATH`:

    ```bash
    export LORE_AUTH_STORE=fallback LORE_AUTH_PATH=/var/lib/lore-machine/auth
    lore auth login lores://lore.example.com:41337 --token-type api-key --token "$MACHINE_API_KEY"
    ```

    With client credentials, log in with `--client-id` instead. Keep this login the only one in the token store: the `remote` mutable store authenticates as the first identity stored there that the upstream accepts, and the `remote` immutable store does too unless its `auth_url` names one (see [Remote store mode](../reference/lore-server-config.md#remote-store-mode)).

2. **Create one socket directory per container.**

    Each container mounts its own directory, so it can't reach another container's socket:

    ```bash
    mkdir -p /run/lore-machine/build-1 /run/lore-machine/build-2
    ```

    On Unix the server leaves the directory's permissions to you, and each socket file takes the bits `socket_mode` sets, `0o666` by default. The user a container runs as must be able to traverse its directory.

3. **Write the server configuration.**

    Create `/etc/lore-machine/config/local.toml`:

    ```toml
    # The upstream's authorization service: the server checks each container's
    # session grants there, per partition.
    [environment.endpoint]
    auth_url = "ucs-auth://auth.example.com"

    # The upstream's token issuer and audience, so the server accepts the
    # tokens the containers already hold.
    [server.auth]
    jwt_issuer = "https://accounts.example.com"
    jwt_audience = ["lore-service"]

    [server]
    upstream_connections = 4

    # The containers use the sockets alone.
    [server.quic]
    enabled = false

    [server.http]
    enabled = false

    [server.grpc]
    host = "127.0.0.1"

    [server.uds]
    enabled = true
    sockets = [
      "/run/lore-machine/build-1/lore.sock",
      "/run/lore-machine/build-2/lore.sock",
    ]

    # Every host the containers' requests name: the upstream, its
    # authentication service, and each service host the upstream advertises.
    [server.uds.forward]
    "lore.example.com" = "https://lore.example.com:41337"
    "auth.example.com" = "https://auth.example.com"

    [immutable_store]
    mode = "composite"

    [immutable_store.composite]
    cache_metadata = true

    [immutable_store.composite.local]
    mode = "local"

    [immutable_store.composite.local.local]
    path = "/var/lib/lore-machine/store"
    max_size = 500000000000
    max_capacity = 50000000

    [immutable_store.composite.durable]
    mode = "remote"

    [immutable_store.composite.durable.remote]
    remote_url = "lores://lore.example.com:41337"

    [mutable_store]
    mode = "remote"

    [mutable_store.remote]
    remote_url = "lores://lore.example.com:41337"
    ```

    What to adjust:

    - `[server.auth]` and the authorizer. The sockets require `[server.auth]` and an authorizer that grants per partition: `auth_url` as above, or `resource_claim` for partition-scoped OIDC tokens. See [Authentication](../reference/lore-server-config.md#authentication) and [Container sockets](../reference/lore-server-config.md#container-sockets).
    - `forward`. Name each host by its bare host name, with no scheme or port. A request for a host the table leaves out fails with `PERMISSION_DENIED`. See [Forwarded gRPC](../reference/lore-server-config.md#forwarded-grpc).
    - The local tier. Set `max_size` to the bytes its disk can spare and `max_capacity` to the entries whose index the server can keep in memory; without `max_capacity` it never evicts. See [Local store mode](../reference/lore-server-config.md#local-store-mode).
    - Verification. The `remote` tier trusts the upstream and checks only each payload's sizes, while the containers' clients check what they fetch. Set `verify = true` in `[immutable_store.composite.durable.remote]` to have the server check each payload too.
    - `upstream_connections`. The storage connections to the upstream that all the containers' requests share, from `1` to `10`.

    The server keeps a gRPC listener of its own, which the containers don't use; binding it to `127.0.0.1` keeps it off the network.

4. **Start the server.**

    Start it as the server's user, with the token store from step 1, and under the machine's service manager so it restarts if it stops:

    ```bash
    LORE_AUTH_STORE=fallback LORE_AUTH_PATH=/var/lib/lore-machine/auth \
      loreserver --config /etc/lore-machine/config
    ```

    Don't set `LORE_UDS_SOCKET` for the server: it refuses to start with it. Once its sockets are bound, it logs `Lore socket server listening` with the number of sockets, and each socket file exists:

    ```bash
    ls -l /run/lore-machine/*/lore.sock
    ```

5. **Start each container with its socket.**

    Mount the container's socket directory and set `LORE_UDS_SOCKET` to the socket's absolute path inside the container. Every Lore process reads the variable from its own environment, the Lore service included, so set it for the whole container:

    ```bash
    docker run --detach --name build-1 \
      --network none \
      --volume /run/lore-machine/build-1:/run/lore \
      --env LORE_UDS_SOCKET=/run/lore/lore.sock \
      <image> <command>
    ```

    Mount the directory, not the socket file: the server binds a new socket at each start, and a mounted file would keep the old one. The container needs no network: its storage and gRPC requests, logins with an API key included, go through the socket. Only OIDC logins in a browser or with client credentials go to the network.

6. **Give each container its tokens.**

    Mount a token store the container may use and point `LORE_AUTH_PATH` at it, or log in through the socket:

    ```bash
    docker exec build-1 lore auth login lore-unix://lore.example.com:41337 \
      --token-type api-key --token "$USER_API_KEY"
    ```

    The machine server checks each storage session against these tokens, and the upstream checks every forwarded request, so a container reaches only the repositories its tokens grant.

7. **Clone through the machine server.**

    In the container, name the upstream's host and port in a `lore-unix` URL:

    ```bash
    docker exec build-1 lore clone lore-unix://lore.example.com:41337/my-project /work/my-project
    ```

## Result

The clone completes in the container, and the content it read is in the machine server's local tier, where the next container's clone of it reads it from the machine's disk.

## Add or remove a container

The server reads `sockets` when it starts. Add or remove the container's path, then restart the server. Clients in the other containers reconnect with back-off once the server answers again, and resend what the restart cut off.

## Troubleshooting

- **`a lore-unix remote is reached through the machine server's socket, which LORE_UDS_SOCKET must name`.** The process that ran the command lacks the variable. A Lore service started without it keeps running without it: stop it with `lore service stop`, and start it again with the variable set.
- **`PERMISSION_DENIED`: `<host> is not forwarded`.** The upstream advertises a host that `[server.uds.forward]` doesn't name. Add the host and restart the server.
- **Clones or pushes fail with a store failure, and the server logs a refusal from the upstream.** The machine identity lacks a grant for the repository, or its login lapsed. Grant it the repository, or log it in again as in step 1. The server takes the change without a restart.
- **The server doesn't start.** Its log names the cause: a socket another server holds, a missing `[server.auth]` or per-partition authorizer, an empty `sockets` list, a `forward` key with a scheme or port, or `LORE_UDS_SOCKET` set in its environment.

## See also

- [Lore Server configuration reference: Container sockets](../reference/lore-server-config.md#container-sockets) — every `[server.uds]` setting, its limits and its startup checks.
- [Lore CLI configuration reference: Environment variables](../reference/lore-cli-config.md#environment-variables) — `LORE_UDS_SOCKET` and what it changes in a process.
- [Machine-level Lore server proposal](../proposals/2026-10-12-machine-level-lore-server-for-containers.md) — why the machine server works this way.
