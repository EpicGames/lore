# Run Lore commands through the background service

In this tutorial you'll turn on the Lore background service, watch a command be carried out by it instead of by the process you ran, and turn it off again. When you're finished the service will be serving every Lore command you run, and you'll know how to override that for a single command.

The service and both settings are per user: the config is your user-level `config.toml`, and the socket lives in a directory private to your user. Another user on the same machine has their own, unaffected by anything here.

## Prerequisites

- A `lore` executable, version 0.10.0 or later. See [install the Lore CLI](../how-to/install-lore-cli.md).
- A Lore repository you can run a read-only command in, such as the one from the [quickstart](quickstart.md).
- Linux, macOS, or Windows. The service talks to its clients over a local socket, which Lore needs one of on the platform.

## Step 1 — Name the build that serves your commands

Lore never guesses which executable to start as the service, so you name it once. Use the absolute path of the `lore` you want serving your commands.

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
lore service set-executable /usr/local/bin/lore
```

```text
Lore service executable set to /usr/local/bin/lore
```

<!-- tab -->
**Windows**

```powershell
lore service set-executable 'C:\Program Files\Lore\lore.exe'
```

```text
Lore service executable set to C:\Program Files\Lore\lore.exe
```

<!-- tabs:end -->

## Step 2 — Turn the service on

```bash
lore service set-use-automatically true
```

```text
Lore commands will be carried out by the service
```

Both settings are needed. With the setting on and no executable named, Lore tells you so and keeps running commands in the process you ran.

## Step 3 — Run a command

Nothing is listening yet, so this starts a service and runs there. Use any repository path you have.

```bash
lore --repository ~/my-project status
```

The command behaves exactly as it did before. What changed is where it ran: the service that started here outlives the command and serves the next one too.

## Step 4 — Stop the service, and start one without running a command

`lore service start` asks for a service to be running and reports the one that is, whether or not it had to start it. `lore service stop` returns only once the socket is free, so nothing has to wait afterwards.

```bash
lore service stop
lore service start
```

```text
Lore service is running
```

## Step 5 — Override the setting for one command

`LORE_USE_SERVICE` decides one command without changing the stored setting. It reads `0`, `false`, `no` and `off` as off.

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
LORE_USE_SERVICE=0 lore --repository ~/my-project status
```

<!-- tab -->
**Windows**

PowerShell has no per-command environment prefix, so set the variable in a child process. Setting it in your own session would overwrite an override you had already set there, and clearing it afterwards would not put that back.

```powershell
powershell -NoProfile -Command '$env:LORE_USE_SERVICE = "0"; lore --repository ~/my-project status'
```

<!-- tabs:end -->

> [!NOTE]
> `lore service run` is the service rather than a client of one, so it always does the work itself whatever these settings say. Use it to run a service in the foreground and watch its output; Ctrl+C stops it cleanly and releases its socket, as `lore service stop` does.

## Verify

Stopping twice is what shows the first stop found a service and the second had none to find:

```bash
lore service start
lore service stop
lore service stop
```

```text
Lore service is running
No Lore service is running
```

Both settings are stored in the user-level `config.toml`, so they hold for later commands:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```toml
[service]
executable = "/usr/local/bin/lore"
use_automatically = true
```

<!-- tab -->
**Windows**

A single-quoted TOML string, so the backslashes stand for themselves rather than starting an escape.

```toml
[service]
executable = 'C:\Program Files\Lore\lore.exe'
use_automatically = true
```

<!-- tabs:end -->

## Troubleshooting

### Commands still run in the process that ran them

Lore prints a warning naming what is missing:

```text
[Warn] No service executable is named, so commands will keep running in the process
that runs them.
```

Relaying needs the setting *and* an executable. Run `lore service set-executable <path>` to complete the pair. Clearing the executable with `lore service set-executable ""` is the other way back to a pair that does not relay.

### `Lore service unavailable`, exit code 32

```text
[Error] Failed to send command to Lore service because: Lore service unavailable:
starting /opt/lore/bin/lore failed: No such file or directory (os error 2)
```

The command never ran, because no service could be reached and none could be started from the executable that is named. Check that the path in `[service] executable` still exists — this is what you see after a build directory is cleaned or a version is uninstalled. Exit code `32` means the service was unreachable, distinct from the command itself having run and failed, so a script can retry it after starting a service.

### A test run or a second checkout keeps stopping your service

Every service belonging to a user answers on the same socket by default, which is what makes one service serve every command you run. Set `LORE_SERVICE_SOCKET` to give a group of processes a service of their own:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
export LORE_SERVICE_SOCKET=lore_service-my-checkout
```

<!-- tab -->
**Windows**

```powershell
$env:LORE_SERVICE_SOCKET = 'lore_service-my-checkout'
```

<!-- tabs:end -->

The value names a single file, not a path. Lore refuses anything containing a path separator and uses the default instead.

## Next steps

- [`[service]` table in the CLI configuration reference](../reference/lore-cli-config.md#service-table) — both fields, the order the executable is resolved in, and the environment variables that override them.
- [`lore service` in the CLI command reference](../reference/lore-cli-commands.md#lore-service) — every subcommand and its flags.
