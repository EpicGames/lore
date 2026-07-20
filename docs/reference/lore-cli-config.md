# Lore CLI configuration reference

## Synopsis

```text
<repo>/.lore/config.toml   # per-repository client settings (created on init/clone)
~/.config/lore/config.toml # user-level global settings (OS user config dir; Linux shown)
~/.config/lore/cli.toml    # user-level CLI settings (OS user config dir; Linux shown)
```

This page documents the **Lore CLI** client configuration: the per-repository `config.toml`, the user-level global `config.toml`, and the user-level `cli.toml` that the `lore` binary reads. These are distinct from the Lore Server daemon's configuration — for server stores, endpoints, topology, and plugin backends, see the [Lore Server configuration reference](lore-server-config.md). The fields below are written by `lore repository create` and `lore clone`, or you edit them by hand; you don't need to read the source to look one up.

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

Set at create or clone time from the URL you pass (for example `lore://127.0.0.1:41337/my-project`). A repository created fully offline may leave it empty. See the [Quickstart](../tutorials/quickstart.md) for how the remote URL is introduced.

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

The `[file]` table controls how the client writes files into the working tree. All three fields are Boolean and default to `false`.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `direct_write` | Boolean | `false` | Write to target files directly instead of writing a temporary file and moving it into place. Writes may not be atomic, so an error can leave a file in an inconsistent state. |
| `direct_io` | Boolean | `false` | Use direct file read and write calls instead of memory-mapping files. |
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

## User-level global `config.toml`

### Location

The global `config.toml` holds settings that apply to every repository, rather than to one. It shares the OS user config directory with `cli.toml`, so on a typical Linux setup it is `~/.config/lore/config.toml`; see the table under `cli.toml` below for the other platforms. Setting `LORE_GLOBAL_PATH` moves the whole directory, which is how the test suite isolates it.

The file is optional. When it is absent, every setting below takes its default.

### Fields

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `use_shared_store_automatically` | bool | `false` | Whether `lore repository create` and `lore clone` configure a shared store without being asked. Read only when a repository is created or cloned; the result is written into that repository's own config. |
| `use_service_automatically` | bool | `false` | Whether Lore runs every command in the background service process. See below. |
| `default_shared_stores` | table | empty | Per-remote default shared store paths, keyed by remote URL. |

### Running commands through the service

With `use_service_automatically` enabled, Lore sends each command to a background service process over a local socket instead of executing it in the CLI process, and starts that service automatically if it is not already running. Set it with:

```bash
lore service set-use-automatically true
```

`lore service start` and `lore service stop` control the process explicitly. Starting when a service is already running, and stopping when none is, are both no-ops rather than errors.

The `LORE_USE_SERVICE` environment variable overrides the setting for a single invocation: any value other than `0` or `false` forces the service on, and `0`, `false`, or an empty value forces it off.

Automatic start-up only applies to the `lore` CLI. When Lore is embedded as a library the running executable is the host application, which Lore will not relaunch as a service; start the service separately in that case.

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
