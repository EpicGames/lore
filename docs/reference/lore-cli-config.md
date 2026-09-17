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

With no `remote_url`, commands that need the network fail with `NoRemote` rather than attempting a connection. That is a distinct error from `Disconnected`, which means a remote *is* configured but could not be reached.

Treat the field as fixed once the repository exists. Pointing an established repository at a different remote is not generally safe — the shared store is keyed on this value, among other things — so create or clone against the remote you intend to use rather than editing this afterwards. See the [Quickstart](../tutorials/quickstart.md) for how the remote URL is introduced.

#### `identity`

`identity` is resolved once, when the repository is created or cloned, and written into `config.toml` — it isn't refreshed on later commands. The resolution order is:

- If you pass `--identity` to `lore repository create` or `lore clone`, that value is written.
- Otherwise Lore uses the identity resolved from the server connection during create or clone.
- If neither resolves to a non-empty value (for example, an offline create with no `--identity`), the field is left unset.

When `identity` is unset and you try to commit, Lore fails with: `No commit identity configured; pass --identity or set identity in .lore/config.toml`. Set the field by hand, or pass `--identity` on the command, to resolve it.

### `[store]` table

The `[store]` table tunes the repository's local stores — the on-disk immutable store (content-addressed fragments) and how much disk it may use before eviction and compaction reclaim space.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `max_capacity` | integer (bytes) | `10485760` (10 MiB) | Maximum capacity of the local immutable store in bytes. |
| `eviction_delay` | integer (seconds) | `10` | Delay before evicting over-capacity fragments. |
| `max_size` | integer (bytes) | `10737418240` (10 GiB) | Maximum total store size in bytes before background compaction reclaims space. |
| `compaction_delay` | integer (seconds) | `30` | Delay between background compaction passes. |
| `verify_write` | Boolean | unset (behaves as `false`) | Verify each write by reading the data back and rehashing it. |

The **Default** column shows the value `lore repository create` and `lore clone` write into a freshly generated `config.toml`. A new repository's `[store]` table already contains every field at these values, so editing the file means changing values that are already present rather than adding missing ones.

#### `verify_write`

`verify_write` is optional. When the key is absent, write verification is off — the same as setting it to `false`. Set it to `true` to have the store re-read and rehash every fragment it writes, trading throughput for an integrity check.

### `[file]` table

The `[file]` table controls how the client writes files into the working tree. Both fields are Boolean and default to `false`.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `direct_write` | Boolean | `false` | Write to target files directly instead of writing a temporary file and moving it into place. Writes may not be atomic, so an error can leave a file in an inconsistent state. |
| `flush_write` | Boolean | `false` | Flush file data to disk after each write. Parsed from the config but not currently wired to any write path, so setting it has no effect today. |

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

Lore normally writes this table for you when you clone with `--use-shared-store`. For how shared stores work and how to set one up, see [Step 6 of the Quickstart](../tutorials/quickstart.md#step-6-set-up-a-shared-store-and-clone-a-second-working-tree); for the `lore clone` and `lore shared-store` flags, see the [Lore CLI command reference](lore-cli-commands.md).

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

### `[service]` table

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `executable` | string | unset | Path of the `lore` executable to run as the [background service](lore-cli-commands.md#lore-service). |
| `use_automatically` | boolean | `false` | Whether commands are carried out by the service rather than in the process you ran. |

**Both are required.** Commands are carried out by the service only when `use_automatically` is on *and* an executable is named. Turning the setting on by itself changes nothing, and Lore says so on each command rather than leaving you to wonder.

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

`lore service set-executable` with an empty path clears the setting, which also stops commands being carried out by the service.

Naming the executable is required because it decides which build serves the machine. A command that finds no service running starts one, so without a name that would be whichever program relayed first — an editor's bundled plugin as readily as the client you installed, and every client on the machine served by it thereafter. Requiring the name makes the version serving a machine something you chose and can read back, rather than an accident of ordering. This matters most where clients or plugins of different versions share a machine.

Lore resolves the executable to start in this order, and the order is stable — a client released later still reads this setting, which is what lets an existing installation be pointed at a specific build after the fact:

1. The `LORE_SERVICE_EXECUTABLE` environment variable, which overrides the setting for a single command. Use it for a build under test, not as a permanent choice.
2. `[service] executable` in this file.
3. The running program, when it is the `lore` client itself.
4. A `lore` executable in the same directory as the running program. This is the case for a program that links `liblore` and ships the client beside it.

Steps 3 and 4 serve `lore service start`, which asks for a service outright and so needs no name. They are not enough for commands to be carried out by the service automatically: that needs step 1 or 2.

If none of those resolve, starting a service fails and names both places one can be set. Running `lore service run` by hand always serves from the build you ran, whatever this setting says; the service reports on startup when that isn't the configured one.

`use_automatically` is what turns the service on and leaves it on. `LORE_USE_SERVICE` overrides it for a single command, and reads `0`, `false`, `no` and `off` as off — so `LORE_USE_SERVICE=0 lore status` runs in the process you ran even where the setting turns the service on. A blank value reads as unset and defers to the setting, as a blank `executable` does. Turning it on through the environment still requires an executable to be named, in either of the two places above.

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
max_capacity = 1073741824   # 1 GiB
eviction_delay = 30
```

A repository created by `lore repository create` or `lore clone` already has every `[store]` field populated at its written default, so you edit values that are already present. The `[store]` example above is shown trimmed for clarity.

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
- [Lore CLI command reference](lore-cli-commands.md) — the `lore clone`, `lore repository create`, and `lore shared-store` flags that write these files.
