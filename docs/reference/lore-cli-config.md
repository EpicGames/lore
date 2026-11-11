# Lore CLI configuration reference

## Synopsis

```text
<repo>/.lore/config.toml   # per-repository client settings (created on init/clone)
~/.config/lore/cli.toml    # user-level CLI settings (OS user config dir; Linux shown)
```

This page documents the **Lore CLI** client configuration: the per-repository `config.toml` and the user-level `cli.toml` that the `lore` binary reads. These are distinct from the Lore Server daemon's configuration — for server stores, endpoints, topology, and plugin backends, see the [Lore Server configuration reference](lore-server-config.md). The fields below are written by `lore repository create` and `lore clone`, or you edit them by hand; you don't need to read the source to look one up.

## Per-repository `config.toml`

### Location

The per-repository config lives at `.lore/config.toml`, inside the repository's `.lore/` folder. Lore creates it when you initialize a repository with `lore repository create` or clone one with `lore clone`, writing the remote URL, your identity, and the default `[store]` and `[file]` tables. The file is the path the client reads on every command; if it's missing, the client uses defaults for everything.

All keys are snake_case. The config structs are plain serde with no case renaming, so `max_capacity`, `eviction_delay`, and the rest appear exactly as written here.

### Top-level fields

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `remote_url` | string | none | URL of the remote repository this working tree pushes to and clones from. |
| `identity` | string | none (resolved at create/clone) | The identity recorded on commits you make from this repository. |

#### `remote_url`

Set at create or clone time from the URL you pass (for example `lore://127.0.0.1:41337/my-project`). It may be empty: a repository created with `--offline` (or `--local`) has no remote, and the argument you pass names the repository rather than locating it — `lore --offline repository create my-project`, or no argument at all to name it after the current directory.

The scheme decides whether the client authenticates the server. `lores://` validates the server certificate, and is the scheme used when the URL has none. `lore://` doesn't: the client accepts any certificate on QUIC connections, and sends gRPC requests to the server, access tokens included, without TLS. Use `lore://` only for a server on the same machine, as in the examples on this page. With a `lores://` remote, the client refuses to connect if the server advertises a storage, revision, lock, repository, or notification endpoint whose scheme doesn't authenticate the server.

A `lore-unix://<upstream host>/<repository>` remote reaches a [machine server](../glossary.md#machine-server), a Lore server on the same machine that fronts the [upstream](../glossary.md#upstream) the host names, through the socket `LORE_UDS_SOCKET` names (see [Environment variables](#environment-variables)). Nothing on the socket uses TLS.

With no `remote_url`, commands that need the network fail with `NoRemote` rather than attempting a connection. That is a distinct error from `Disconnected`, which means a remote *is* configured but could not be reached.

Treat the field as fixed once the repository exists. Pointing an established repository at a different remote is not generally safe — the shared store is keyed on this value, among other things — so create or clone against the remote you intend to use rather than editing this afterwards. See the [Quickstart](../tutorials/quickstart.md) for how the remote URL is introduced.

#### `identity`

`identity` is resolved once, when the repository is created or cloned, and written into `config.toml` — it isn't refreshed on later commands. The resolution order is:

- If you pass `--identity` to `lore repository create` or `lore clone`, that value is written.
- Otherwise Lore uses the identity resolved from the server connection during create or clone.
- If neither resolves to a non-empty value (for example, an offline create with no `--identity`), the field is left unset.

When `identity` is unset and you try to commit, Lore fails with: `No commit identity configured; pass --identity or set identity in .lore/config.toml`. Set the field by hand, or pass `--identity` on the command, to resolve it.

### `[store]` table

The `[store]` table tunes the repository's local stores — the on-disk immutable store (content-addressed fragments) and how much it may hold before eviction and compaction reclaim space.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `max_capacity` | integer (fragments) | `2000000` | Maximum number of fragments in the local immutable store. Eviction removes the least recently used fragments to hold the store near 70% of it. The periodic eviction pass treats a value below `1048576` as `1048576`. |
| `eviction_delay` | integer (seconds) | `30` | Interval between background eviction passes. The first pass runs one interval after the store opens. |
| `max_size` | integer (bytes) | `10737418240` (10 GiB) | Maximum total size of the local immutable store's packfiles. Above it, compaction evicts the least recently used content toward 70% of it and rewrites packfiles to reclaim the space. |
| `compaction_delay` | integer (seconds) | `50` | Interval before a background compaction pass starts from the beginning. A pass that an earlier command stopped after its first step resumes as soon as the store opens. |
| `verify_write` | Boolean | unset (behaves as `false`) | Verify each write by reading the data back and rehashing it. |
| `cache_payloads` | Boolean | unset (behaves as `false`) | Keep the content of files commands fetch from the remote, and of those they write, in the store. Instances that share a store want this, so a second instance at the same revision reads the content locally instead of fetching it again. |

A command that finds the store over a limit as it loads it starts a pass at once rather than after the interval.

Eviction removes only content the remote also holds, which Lore fetches again when it's next read. Content held only locally, such as commits not yet pushed, counts toward both limits but is never evicted. Once it fills about 70% of a limit, eviction removes the remote content cached beside it. The store keeps its packfiles in 256 groups, and compaction rewrites a group's packfiles only once at least 10% of their bytes are no longer referenced.

The **Default** column shows the value `lore repository create` and `lore clone` write into a freshly generated `config.toml`. A new repository's `[store]` table already contains every field but `verify_write` and `cache_payloads` at these values, so editing the file means changing values that are already present rather than adding missing ones.

#### `verify_write`

`verify_write` is optional. When the key is absent, write verification is off — the same as setting it to `false`. Set it to `true` to have the store re-read and rehash every fragment it writes, trading throughput for an integrity check.

### `[file]` table

The `[file]` table controls how the client writes files into the working tree. Both fields are Boolean and default to `false`.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `direct_write` | Boolean | `false` | Write to target files directly instead of writing a temporary file and moving it into place. Writes may not be atomic, so an error can leave a file in an inconsistent state. |
| `flush_write` | Boolean | `false` | Flush file data to disk after each write. Parsed from the config but not currently wired to any write path, so setting it has no effect today. |

### `[sync]` table

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `prefetch_tree` | Boolean | `false` | Before a sync walks the target revision, fetch all of its tree blocks and name tables in bulk requests instead of one per block as the walk reaches it. This fetches the whole tree whatever the view, so turn it on for instances whose syncs walk most of a large tree. |

### Shared-store table

A repository can point its immutable store at a *shared store* so working trees cloned from the same server store content only once on disk — deduplicated at the fragment level, across similar files and not just byte-identical ones. Those settings live in the `shared_store_to_use` table.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `use_shared_store` | Boolean | unset | Whether this repository uses a shared store rather than its own `.lore/` store. |
| `shared_store_path` | string | unset | Filesystem path of the shared store to use. When unset, Lore uses the system default shared-store location. |

The table key and both fields accept legacy serde aliases for backward compatibility with configs written by older clients:

| Current name | Legacy alias |
| --- | --- |
| `shared_store_to_use` (table) | `global_store_to_use` |
| `use_shared_store` | `use_global_store` |
| `shared_store_path` | `global_store_path` |

A config that uses the legacy names still loads. New configs use the current names.

Lore normally writes this table for you when you clone with `--use-shared-store` or `--vfs`, or with automatic use turned on by `lore shared-store set-use-automatically true`. For how shared stores work and how to set one up, see [Step 6 of the Quickstart](../tutorials/quickstart.md#step-6-set-up-a-shared-store-and-clone-a-second-working-tree); for running and maintaining many instances over one store, see [Run parallel instances over a shared store](../how-to/run-parallel-instances-over-a-shared-store.md); for the `lore clone` and `lore shared-store` flags, see the [Lore CLI command reference](lore-cli-commands.md).

## User-level `cli.toml`

### Location

`cli.toml` lives in the OS user config directory — **not** in any project's `.lore/` folder. Lore resolves the directory with the [`directories`](https://docs.rs/directories) crate's `config_local_dir()`, under an application directory named for Lore. On a typical Linux setup that's `~/.config/lore/cli.toml`. The directory differs by platform:

| Platform | `cli.toml` location |
| --- | --- |
| Linux | `~/.config/lore/cli.toml` (or `$XDG_CONFIG_HOME/lore/cli.toml`) |
| macOS | `~/Library/Application Support/com.epicgames.lore/cli.toml` |
| Windows | `%LOCALAPPDATA%\Epic Games\lore\config\cli.toml` |

The file is optional; when it's absent, Lore uses the defaults below.

### Fields

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `pager` | string | `less -R` (Unix and macOS), `more.com` (Windows) | The pager program Lore pipes long output through. |

`pager` is the only field Lore reads from `cli.toml` today. The CLI's other behavioral settings — JSON output, log level, debug logging, and non-interactive mode — are set per invocation through command-line flags, not through this file. Passing `--no-pager` (or requesting JSON output) overrides `pager` for that command and disables paging.

## User-level `config.toml`

### Location

`config.toml` sits beside `cli.toml` in the same OS user config directory (`~/.config/lore/config.toml` on a typical Linux setup), and holds settings that apply to every repository rather than to one. It is optional. Don't confuse it with the per-repository `config.toml` documented above, which lives in a repository's `.lore/` folder.

Lore rewrites this file when a command changes a setting in it, such as `lore service set-executable`. It keeps every key and table it doesn't know, such as a setting that a newer version of Lore added. It doesn't keep comments or the order of keys.

### `[service]` table

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `executable` | string | unset | Path of the `lore` executable to run as the [background service](lore-cli-commands.md#lore-service). |
| `use_automatically` | boolean | `false` | Whether commands are carried out by the service rather than in the process you ran. |
| `idle_connection_timeout` | integer (seconds) | `300` | Seconds the service keeps a connection to a remote open once no command is using it. |

**Set both to have commands carried out by the service.** With `use_automatically` on, every command goes to the service. A command that finds none running starts one from the named executable, and without one fails with exit code 32 rather than run in the process you ran. Turning the setting on without naming an executable makes Lore say so.

```toml
[service]
executable = "/opt/lore/1.9/bin/lore"
use_automatically = true
```

Set both with commands rather than by hand:

```bash
lore service set-executable /opt/lore/1.9/bin/lore
lore service set-use-automatically true
```

`lore service set-executable` with an empty path clears the setting. Commands that `use_automatically` sends to the service then still go to a service that's running, but no longer start one.

A command that `use_automatically` sends to the service starts one only from a named executable, because that decides which build serves the machine. Every program on the machine that links Lore follows `use_automatically`, so without a name the service would be whichever program relayed first — an editor's bundled plugin as much as the client you installed, and every client on the machine served by it thereafter. Requiring the name makes the version serving a machine something you chose and can read back, rather than an accident of ordering. This matters most where clients or plugins of different versions share a machine.

Lore resolves the executable to start in this order, and the order is stable — a client released later still reads this setting, which is what lets an existing installation be pointed at a specific build after the fact:

1. The `LORE_SERVICE_EXECUTABLE` environment variable, which overrides the setting for a single command. Use it for a build under test, not as a permanent choice.
2. `[service] executable` in this file.
3. The running program, when it is the `lore` client itself.

Step 3 applies in two cases. One is `lore service start`, which asks for a service outright and so needs no name. The other is a command that makes a [virtual clone](../how-to/use-virtual-clones-on-linux.md), or works in one whose service ended without taking its mount down, which only the service can carry out. A command that `use_automatically` sends to the service doesn't start one through step 3: that needs step 1 or 2. Step 3 applies to the `lore` client alone, so a program that links the library starts a service only from step 1 or 2.

If none of those resolve, starting a service fails and names both places one can be set. Running `lore service run` by hand always serves from the build you ran, whatever this setting says; the service reports on startup when that isn't the configured one.

`use_automatically` is what turns the service on and leaves it on. `LORE_USE_SERVICE` overrides it for a single command, and reads `0`, `false`, `no` and `off` as off — so `LORE_USE_SERVICE=0 lore status` runs in the process you ran even where the setting turns the service on. A blank value reads as unset and defers to the setting, as a blank `executable` does. A command it turns on starts a service only from an executable named in either of the first two places above.

`idle_connection_timeout` decides how long a connection outlives the commands that use it. A command to a remote the service already has a connection to reuses it, rather than connecting and authenticating again. Once no command has used a connection for this many seconds, the service closes it, and the server releases it. It closes within a minute after the timeout has passed, and the next command to that remote connects again. With `0`, a connection closes within a second or two after the last command using it finishes. A connection that a mount or a notification subscription holds stays open for as long as that holds it. The service reads the setting when it starts, and so does a program that runs the service through the C API.

```toml
[service]
idle_connection_timeout = 600
```

### `[auth]` table

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `store` | `secure` or `fallback` | `secure` | Where the token store's encryption key is kept: `secure` for the operating system's secure store, `fallback` for a file beside the token store, which the store writes for its owner alone to read (on Unix, mode 0600; on Windows, an access list of its own granting its owner). Any other value is an error. `fallback` suits machines where nothing can answer a secure-store prompt, such as build agents, and binaries rebuilt with a new signature, which the secure store treats as new applications. |
| `path` | string | unset | Directory holding the token store, in place of the per-user default. A relative path is relative to the directory that holds `config.toml`, whatever directory you run a command from. |

`LORE_AUTH_STORE` and `LORE_AUTH_PATH` override them for a single command. `LORE_AUTH_STORE` takes the same two values, and a blank variable counts as unset. A command carried out by the service uses the choice of the process you ran, and the secure store where that process made none.

Only the `lore` command applies `[auth]`. A program that links the Lore library uses the two variables alone.

A token store directory holds the logins of one key, so `fallback` needs a directory of its own, from `path` or `LORE_AUTH_PATH`: without one, a command stops with an error rather than replacing the key of the default token store, which programs using the secure store share. Changing `store` for a directory can lose its logins: going to `fallback`, they're reset, and going to `secure`, those the secure store's key didn't seal can't be opened.

A user-level `config.toml` that can't be read, isn't TOML, or holds an `[auth]` table that doesn't parse stops every `lore` command with an error naming the file. Other tables don't affect it.

### Giving a run its own service

| Variable | Effect |
| --- | --- |
| `LORE_SERVICE_SOCKET` | Names the socket a service listens on. Processes sharing a value share a service; processes with different values get one each. |

Every service belonging to a user answers on the same socket by default, which is what makes one service serve every command that user runs. It is also why a test suite, or a second checkout, would otherwise take over the service already running: stopping and starting one affects whatever else was using it.

Set `LORE_SERVICE_SOCKET` to give a group of processes a service of their own. The value names a single file, not a path — anything containing a path separator is refused in favour of the default rather than honoured, since the socket's directory is chosen for being private to your user and a path could move it somewhere with weaker permissions.

```bash
export LORE_SERVICE_SOCKET=lore_service-my-checkout
```

The Python test suite sets this per run, so running it leaves a service you have running alone.

## Environment variables

`LORE_SERVICE_EXECUTABLE`, `LORE_USE_SERVICE` and `LORE_SERVICE_SOCKET` choose the Lore service that carries out commands; the [`[service]` table](#service-table) and [Giving a run its own service](#giving-a-run-its-own-service) describe them. The variable below decides how a process reaches its remote.

| Variable | Effect |
| --- | --- |
| `LORE_UDS_SOCKET` | Names the socket of a machine server. Every gRPC channel the process opens goes through it, and a `lore-unix` remote requires it. |

`LORE_UDS_SOCKET` holds the absolute path of a socket a machine server listens on, typically one bind-mounted into a container (see [Container sockets](lore-server-config.md#container-sockets)). With it set, every gRPC channel the process opens, the authentication service's included, connects through the socket in plaintext and keeps the host it would have dialed as the request's authority, which the machine server forwards the request by. A `lore-unix` remote also sends its storage requests through the socket, over one storage connection per process. The process sizes its network threads for that one connection. OIDC login requests, which are HTTP, still go to the network.

- A `lore-unix` remote without the variable fails with an error that names it. An empty value reads as unset.
- The process uses the path as given, so a relative path resolves against the working directory of each process that connects.
- A process with the variable set reaches over gRPC only the hosts the machine server forwards to. It can use a remote of another scheme as well only if the machine server forwards that remote's host and the process has network access for the remote's QUIC storage, which it reaches over one storage connection. In a container with no network, as usual, only `lore-unix` remotes work.
- Each process reads the variable once, from its own environment. The relay does not pass it to the Lore service, so set it for the service as well.
- A Lore server refuses to start with the variable set in its own environment, since the variable would send the server's own gRPC into a socket (see [Container sockets](lore-server-config.md#container-sockets)).

## Examples

### Minimal `config.toml`

A repository config with just a remote URL and an identity:

```toml
remote_url = "lore://127.0.0.1:41337/my-project"
identity = "alex@example.com"
```

### Cap the local store size

Override the `[store]` defaults to give this repository a larger local immutable store and a longer eviction delay:

```toml
remote_url = "lore://127.0.0.1:41337/my-project"
identity = "alex@example.com"

[store]
max_size = 53687091200    # 50 GiB
max_capacity = 10000000   # fragments
eviction_delay = 60
```

A repository created by `lore repository create` or `lore clone` already has every `[store]` field but `verify_write` and `cache_payloads` populated at its written default, so you edit values that are already present. The `[store]` example above is shown trimmed for clarity.

### Use a shared store

Point this working tree at a shared store at a specific path:

```toml
remote_url = "lore://127.0.0.1:41337/my-project"
identity = "alex@example.com"

[shared_store_to_use]
use_shared_store = true
shared_store_path = "/srv/lore/shared-store"
```

### Set a custom pager in `cli.toml`

Use a different pager for all `lore` commands:

```toml
pager = "bat --paging=always"
```

## See also

- [Lore Server configuration reference](lore-server-config.md) — the `loreserver` daemon's settings, distinct from the client config on this page.
- [Quickstart](../tutorials/quickstart.md) — clone, stage, commit, and push your first revision, and set up a shared store.
- [Run parallel instances over a shared store](../how-to/run-parallel-instances-over-a-shared-store.md) — keep many working trees over one shared store, and move, prune, and remove them.
- [Lore CLI command reference](lore-cli-commands.md) — the `lore clone`, `lore repository create`, and `lore shared-store` flags that write these files.
