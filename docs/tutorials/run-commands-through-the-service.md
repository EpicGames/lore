# Run Lore commands through the background service

Lore can execute your commands in a background service process instead of in the CLI process that you invoked. In this tutorial you'll name the executable to run as the service, start it, run a command through it, make Lore use it automatically for every command, override that for a single command, and stop the service again. By the end you'll know which process is doing the work and how to control it.

## Prerequisites

- The `lore` CLI installed and on your PATH. See [Install the Lore CLI](../how-to/install-lore-cli.md).
- A Lore repository to run commands in. The [Quickstart](quickstart.md) creates one.
- Linux, macOS, or Windows. The service uses a local socket, which Lore supports on all three.

## Step 1 — Name the executable to run as the service

Lore never guesses which binary to launch, so tell it once:

```bash
lore service set-executable /path/to/lore
```

Use the path to the `lore` binary you're running — `which lore` on macOS or Linux, `where lore` on Windows, prints it. This writes `service_executable` into your user-level global `config.toml`, and every start from now on uses it.

## Step 2 — Start the service

From anywhere on your machine:

```bash
lore service start
```

The command returns as soon as the service is listening. It runs detached, with no console and no terminal of its own, so it outlives the shell you started it from.

Run it a second time:

```bash
lore service start
```

It succeeds again. Starting a service that's already running is a no-op rather than an error, so you never need to check first.

## Step 3 — Run a command through the service

Run a command in your repository with `LORE_USE_SERVICE` set:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
LORE_USE_SERVICE=1 lore status
```

<!-- tab -->
**Windows**

```powershell
$env:LORE_USE_SERVICE=1; lore status
```

<!-- tabs:end -->

The output is identical to running `lore status` on its own. What changed is where the work happened: the CLI serialized the command, sent it over the socket, and the service executed it and streamed the results back.

Relative paths resolve against your directory, not the service's. The CLI sends the directory you ran in along with the command, so `lore stage myfile.uasset` stages the file next to you, even though the service runs in a different directory.

## Step 4 — Use the service for every command

To route every command without setting the environment variable each time, enable the setting:

```bash
lore service set-use-automatically true
```

This writes `use_service_automatically = true` alongside the `service_executable` you set in step 1. Now run a command with no environment variable at all:

```bash
lore status
```

It routes through the service. If no service is running, the CLI starts one first.

Both settings are needed. Routing has to be able to start a service, and step 1 is what says how — with `use_service_automatically` on and no executable named, commands keep running in the CLI process rather than failing.

> [!NOTE]
> Both settings are read once per process. The `lore` CLI runs one command per process, so a change takes effect on your next command — but a long-lived application with Lore embedded keeps whatever they were when it started.

## Step 5 — Override the setting for one command

To run a single command in your own process while the setting stays on:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
LORE_USE_SERVICE=0 lore status
```

<!-- tab -->
**Windows**

```powershell
$env:LORE_USE_SERVICE=0; lore status
```

<!-- tabs:end -->

`LORE_USE_SERVICE` overrides `use_service_automatically` in both directions: `1` runs the command in the service, `0` runs it locally. It doesn't override step 1 — forcing the service on still needs an executable to start one from, if none is already running. The [configuration reference](../reference/lore-cli-config.md#running-commands-through-the-service) lists the other values it accepts.

## Step 6 — Stop the service

```bash
lore service stop
```

Like `start`, this is a no-op when nothing is running, so it always succeeds.

> [!WARNING]
> Stopping doesn't wait for in-flight commands. A command still executing when the service stops sees its connection close and reports that the service was stopped. Let long operations finish before stopping the service.

If you turned on automatic use in Step 4 and want to go back to running everything in the CLI process:

```bash
lore service set-use-automatically false
```

## Verify

With the service stopped, start it and look for the process:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
lore service start
pgrep -fl "lore service run"
```

```text
67460 /usr/local/bin/lore service run
```

<!-- tab -->
**Windows**

```powershell
lore service start
Get-CimInstance Win32_Process -Filter "Name = 'lore.exe'" | Select-Object ProcessId, CommandLine
```

```text
ProcessId CommandLine
--------- -----------
    67460 "C:\Program Files\lore\lore.exe" service run
```

<!-- tabs:end -->

A `service run` process you didn't start yourself means `lore service start` worked. Stop it again and the process is gone:

```bash
lore service stop
```

Re-running the process listing prints nothing.

## Troubleshooting

**`cannot start the Lore service: no executable to launch`.** Nothing named an executable, and nothing is inferred from the binary you ran — see step 1. Name one for every start:

```bash
lore service set-executable /path/to/lore
```

Or for the one command:

```bash
lore service start --executable /path/to/lore
```

`lore service set-executable ""` clears the setting again.

**Commands run in the CLI process even with `use_service_automatically` on.** Routing needs an executable to start a service from, so the setting does nothing until `service_executable` names one. Look for `service_executable` in your global `config.toml`, or just run step 1 again.

**A command fails with error code `50`.** That's `LORE_ERROR_CODE_SERVICE_UNAVAILABLE`: the command was routed to the service but never ran, because none was reachable and none could be started. It's distinct from a generic failure so you can tell it apart from the command's own errors. Run `lore service start` and try again, which itself needs step 1 done — and if you reach this from an embedded Lore, either configure `service_executable` as above or pass the Lore binary you ship in the `executable` field of `lore_service_start_args_t`.

**The service isn't supported on this OS.** The service needs local socket support. On a platform without it, run commands locally — `LORE_USE_SERVICE=0`, or leave `use_service_automatically` off.

**A command reports the service closed the connection.** The service stopped or was terminated while your command was in flight. Start the service again and re-run the command.

## Next steps

- [Lore CLI configuration reference](../reference/lore-cli-config.md#running-commands-through-the-service) — the `use_service_automatically` and `service_executable` settings, the other global config fields, and where the file lives on each platform.
- [Lore CLI command reference](../reference/lore-cli-commands.md) — every `lore service` subcommand and flag.
- [Quickstart](quickstart.md) — the core Lore loop, if you don't have a repository to try this in yet.
