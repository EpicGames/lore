# Fortnite from scratch: p4, Lore, Lore over FUSE

Five runs on one machine, each timed from the start of the clone or sync to the end of the
build of the same changelist with the same mappings; the first four from cold:

1. p4 sync of `//Fortnite/Release-43.00`, then the build.
2. Ordinary Lore clone with the view filter, then the build.
3. Lore clone over FUSE with no prefetch, recording the build's file accesses, then the build.
4. Lore clone over FUSE prefetching from that recording, then the build.
5. Lore clone over FUSE with no prefetch over the shared store run 4 fetched, warm, then the build.
6. The build alone in an ordinary clone on local disk, from cold: the baseline the FUSE runs'
   generation and build compare with.

`fortnite-run.sh <run>` makes one run with this directory's scripts and `../m2/`, from a Lore
checkout of this branch at `/data/lore-fuse-vfs`. Everything the runs read and write is under
`/data`. Each run keeps all it produced in a directory of its own, then measures the disk it used
and deletes the rest, a tree being about 1 TiB, so a run that fails is rerun alone.

## The target

`FortniteGame` is the project (`FortniteGame/FortniteGame.uproject`); it has no target of that
name. Its Linux target is `FortniteServer` (`[SupportedPlatforms(UnrealPlatformClass.Server)]`);
`FortniteClient` and `FortniteEditor` carry Linux code paths but declare no Linux support. The runs
below build `TARGET=FortniteServer`; set another target the same way. After the sync or clone,
each run times two steps, the project file generation and the build:

```sh
./GenerateProjectFiles.sh \
    -project=<tree>/FortniteGame/FortniteGame.uproject -game -Log=<run>/gpf-ubt.log -Timestamps
Engine/Build/BatchFiles/RunUBT.sh $TARGET Linux Development \
    -project=<tree>/FortniteGame/FortniteGame.uproject \
    -MaxParallelActions=$M2_ACTIONS -UBARootDir=<fresh directory per run> -Log=<run>/ubt.log -Timestamps
```

The tree's root `GenerateProjectFiles.sh` sets up the debugger helpers (`~/.lldbinit` when it is
missing, `~/.config/Epic/GDBPrinters`), then builds UnrealBuildTool and runs it with
`-projectfiles`. Building UnrealBuildTool compiles every `Engine/Platforms/*` directory's
UnrealBuildTool sources present, so a view keeps a platform whole or excludes it whole: `Switch2`
without `Nintendo` fails it. `../m2/build.sh <tree> <target> <label>`
runs both steps (the generation when `M2_GENERATE_PROJECT_FILES` is set), pinned to `M2_CORES`,
with UBA's store in the run's own directory so no build warms the next. It writes each step's
`<step>_s=<seconds> exit=<status>` to `steps.log`, keeps UnrealBuildTool's own logs, timestamped,
beside the console logs, so the build splits into its phases afterwards, and prints
`<label> <target> <build seconds> <exit> KEEP|DISCARD`. A build or test process seen during it
(`cargo`, `rustc`, `pytest`, ...) marks it `DISCARD`. Keep `~/.config/Unreal Engine/UnrealBuildTool/`
the same for every run: a remote executor or cache configured there changes what is measured.

## Prerequisites

- Linux 6.8 or later (FUSE protocol 7.39), `fusermount3` (package `fuse3`), `curl`, `bc`, `p4`.
- A Lore binary of this branch. Either build it, which needs current stable Rust:

  ```sh
  lore clone lores://lore.internal.epicgames.net/019e3be9efcd761194d2fbcbb44d249f /data/lore-fuse-vfs \
      --branch mjansson/lore-fuse-m3-followups
  cd /data/lore-fuse-vfs && cargo build --profile release-lto -p lore-client
  ```

  or copy `target/release-lto/lore` from a machine that built it, and still clone the checkout for the
  scripts.
- A Lore login to the host: `lore login lores://lore.internal.epicgames.net`. The login is kept in
  the keyring and `~/.config`, not under `LORE_GLOBAL_PATH`, so an existing one serves.
- Passwordless cache drop, which every run does first. `../m2/cold-vfs.sh` does not check that the
  drop succeeded, so without it the FUSE runs start warm and are still marked `KEEP`:

  ```sh
  echo "$USER ALL=(root) NOPASSWD: /usr/sbin/sysctl -q vm.drop_caches=3" > /data/drop-caches.sudoers
  sudo visudo -cf /data/drop-caches.sudoers    # parsed OK; a broken file in sudoers.d breaks sudo
  sudo install -m 0440 -o root -g root /data/drop-caches.sudoers /etc/sudoers.d/lore-drop-caches
  rm /data/drop-caches.sudoers
  sudo -k; sudo -n /usr/sbin/sysctl -q vm.drop_caches=3 && echo passwordless OK
  ```

  The file name carries no dot: sudo skips files in `/etc/sudoers.d` whose name has one.
- The Linux toolchain the engine names in `Engine/Config/Linux/Linux_SDK.json` (`MainVersion`), on
  `Release-43.00` `v26_clang-20.1.8-rockylinux8`:

  ```sh
  mkdir -p /data/ue-toolchain && cd /data/ue-toolchain
  curl -O https://cdn.unrealengine.com/Toolchain_Linux/native-linux-v26_clang-20.1.8-rockylinux8.tar.gz
  tar xzf native-linux-v26_clang-20.1.8-rockylinux8.tar.gz    # /data/ue-toolchain/v26_clang-20.1.8-rockylinux8/
  rm native-linux-v26_clang-20.1.8-rockylinux8.tar.gz
  ```
- The view filter, here `/data/fortnite-view.txt`. It must keep `/Engine/Source/ThirdParty/**/Linux/`:
  the third-party libraries' Linux builds and headers live there (ICU, Opus, MsQuic, the EOS SDK, ...).
  It excludes `/FortniteGame/Plugins/**/Content/`, the plugins' assets, 753 GiB a compile is not
  expected to read.
- Disk for one tree plus its build outputs. At CL 58890344 the view keeps 265 GiB in 2.3 million
  files (`Engine` 127 GiB, `FortniteGame` 138 GiB); the build outputs come on top, and the first
  runs' `disk_mib` lines measure them. Only one run's data exists at a time: each run's last step
  deletes it.

## Environment

`/data/fn-exp/env.sh`, sourced by the scripts through `M2_ENV`:

```sh
export LORE=/data/lore-fuse-vfs/target/release-lto/lore
export LORE_SERVICE_EXECUTABLE=$LORE
export LORE_GLOBAL_PATH=/data/fn-exp/global      # registry, shared store and external directories of these runs only
export LORE_SERVICE_SOCKET=fn-exp                 # a service of these runs only
export LORE_USE_SERVICE=1
# Lore's log (otherwise ~/.local/share/lore/logs) and temporary files, kept under /data.
export XDG_DATA_HOME=/data/fn-exp/xdg
export TMPDIR=/data/fn-exp/tmp
# UBT's .NET home and package cache (otherwise ~/.dotnet and ~/.nuget), the same for every run.
export DOTNET_CLI_HOME=/data/fn-exp/dotnet-home
export NUGET_PACKAGES=/data/fn-exp/nuget/packages
# MSBuild worker nodes exit with the build that started them: none outlives a run, holding its
# mount, or serves the next run warm.
export MSBUILDDISABLENODEREUSE=1
export LINUX_MULTIARCH_ROOT=/data/ue-toolchain/v26_clang-20.1.8-rockylinux8/
export P4CLIENT=<the client created below>
# Cores and parallel actions of every build.
export M2_CORES=0-$(($(nproc) - 1)) M2_ACTIONS=$(nproc)
# Generate the project files as a timed step of its own before each build (m2/build.sh).
export M2_GENERATE_PROJECT_FILES=1
# The full build, link included; a link-only failure counts as succeeded: the monolithic
# FortniteServer link overflows 32-bit relocations (main, CL 58907763).
export M2_IGNORE_LINK_FAILURE=1
# fortnite-run.sh: where runs land, what they build and where.
export FN_RUNS=/data/fn-exp/runs
export FN_VIEW=/data/fortnite-view.txt
export FN_TARGET=FortniteServer
export FN_CL=<the changelist chosen below>
export FN_MOUNT=/data/fn-exp/mnt/fn
export FN_LORE_TREE=/data/fn-exp/lore
export FN_MIN_FREE_GIB=500                        # one tree, 265 GiB, plus its build outputs
export BENCH_REV=<the revision chosen below>
```

`LORE_GLOBAL_PATH` places Lore's global data directory, and with it the shared store
(`$LORE_GLOBAL_PATH/data/shared_store`), the store registry and the FUSE runs' external
directories, on `/data`. A value set in `env.sh` wins over the caller's: the scripts source it
again. Once:

```sh
mkdir -p /data/fn-exp/runs /data/fn-exp/tmp /data/fn-exp/xdg /data/fn-exp/dotnet-home /data/fn-exp/nuget
cp -a ~/.nuget/packages /data/fn-exp/nuget/    # no build restores packages a later one finds warm
```

In the shell running the experiments, preferably inside `tmux`, since a run takes hours, and not
one used for other Lore work: `env.sh` points every `lore` at the experiments' global path, where
another repository's shared store is not found:

```sh
export M2_ENV=/data/fn-exp/env.sh
source $M2_ENV
B=/data/lore-fuse-vfs/lore-fuse/bench
```

## Choose the revision and its changelist

Every revision of the Fortnite repository records the changelist it imported. A virtual clone of
the `Release-43.00` branch reads it in seconds:

```sh
LORE_TREE_PREFETCH_DEPTH=0 $LORE service start
$LORE clone lore.internal.epicgames.net/Fortnite /data/fn-exp/probe --vfs fuse --use-shared-store \
    --branch Release-43.00
(cd /data/fn-exp/probe && $LORE history 1)    # Signature: the revision; Changelist: the p4 CL
$LORE service stop --force
rmdir /data/fn-exp/probe                      # an empty mount point once the service is stopped
rm -rf "$LORE_GLOBAL_PATH/data/shared_store" "$LORE_GLOBAL_PATH/data/external"
echo 'entries = []' > "$LORE_GLOBAL_PATH/data/stores/registry.toml"    # an entry for a deleted store fails the next clone
```

Put the signature in `BENCH_REV` and the changelist in `FN_CL` in `env.sh`.
Run 1 syncs the changelist; runs 2 to 4 clone `BENCH_REV`, which needs no `--branch`. Runs 1 and
2 of 2026-10-05 used `Release-43.00` at `20d5a6b7…`, changelist 58890344, which does not compile
for `FortniteServer` on Linux (`CoronadoPlayerStateComponent_CosmeticLoadout.cpp:46`, a delegate
signature mismatch). The runs from run 3 on use `main` at `1bd67d7f…`, changelist 58907763 (the
p4 workspace maps `//Fortnite/Release-43.00`; a p4 run on main needs a workspace of `//Fortnite/Main`).
That changelist compiles, but the monolithic `FortniteServer` link fails (`ld.lld: relocation
R_X86_64_32 out of range`, at offsets just past 4 GiB). The runs build in full, link included, and
`M2_IGNORE_LINK_FAILURE=1` counts a build whose only errors are the linker's as succeeded; its
`steps` line keeps the real exit and adds `link_failure_ignored` (the FUSE runs' line
`link_failed=ignored`).

## The p4 workspace

A stream client of `//Fortnite/Release-43.00` maps everything the stream maps, about 2.4 TiB with
`Samples` and `Sandbox`, which the view leaves out. Run 1 uses a non-stream client with the view's
mappings instead; `view-p4-client.sh` writes its spec from the view and the stream:

```sh
$B/m3/view-p4-client.sh $FN_VIEW //Fortnite/Release-43.00 $P4CLIENT /data/fn-exp/p4 | p4 client -i
```

Each view line becomes an exclusion (`/a/b/` → `-//Fortnite/Release-43.00/a/b/...`, `**/a/` →
`.../a/...`); a top-level directory the view excludes whole is left unmapped, and the stream's own
exclusions are kept. Both sides match case-insensitively. Rerun it after changing the view. Run 1
needs the workspace empty: an empty root and an empty have list, as a new client has and the run's
cleanup leaves.

## Running

```sh
$B/m3/fortnite-run.sh p4
$B/m3/fortnite-run.sh lore
$B/m3/fortnite-run.sh fuse-record
$B/m3/fortnite-run.sh fuse-prefetch
$B/m3/fortnite-run.sh fuse-warm
$B/m3/fortnite-run.sh local-build
```

or in one go, stopping at the first run that does not succeed:

```sh
for run in p4 lore fuse-record fuse-prefetch fuse-warm; do $B/m3/fortnite-run.sh $run || break; done
```

Without `tmux`, detached from the terminal, in a session whose id `all.pid` records:

```sh
setsid nohup bash -c 'echo $$ > /data/fn-exp/runs/all.pid
    for run in p4 lore fuse-record fuse-prefetch fuse-warm; do '"$B"'/m3/fortnite-run.sh $run || break; done' \
    > /data/fn-exp/runs/all.log 2>&1 < /dev/null &
tail -f /data/fn-exp/runs/results.log
kill -INT -- -$(cat /data/fn-exp/runs/all.pid)    # stops it the way Ctrl-C does
```

Each run writes `/data/fn-exp/runs/<run>-<time>/` and exits 0 when it succeeded undisturbed. On
any exit, a failure or Ctrl-C included, it then stops the service, notes the disk the run
occupies and moves the Lore log into the run directory. A run that succeeded, or whose status is
`discard`, then deletes the tree, the shared store, the external directories and the UBA
directories. A run that failed keeps them for investigation and notes what it kept (`kept: ...`).
A run refuses to start while data it would delete or reuse exists, whether a failed run kept it
or a killed one left it: p4 its workspace root, lore its tree and the shared store, the FUSE runs
the shared store and external directories. Once it is investigated, `$B/m3/fortnite-run.sh clean`
deletes all of it. Rerun a run whose `status` is not `ok` with the same command; the failed
attempt's directory stays. Stop a run with Ctrl-C, which reaches the build too: a signal to the
script alone waits for the step running. Before p4 and lore, which write the whole tree, the run
checks for `FN_MIN_FREE_GIB` free.

A run directory holds:

- `results.log`: the run's timing lines (the sync or clone; the build's summary; a `steps` line
  with `gpf_s`, `build_s` and `total_s`, the sync or clone plus both steps), its `disk_mib` line
  (each path the run occupied, in MiB, measured before the cleanup), its status and the space free
  after the cleanup.
- `status`: `ok`, or why the run failed (`failed: ...`) or is not comparable (`discard: ...`).
- `inputs/`: the view, the environment, `lore --version` and, for p4, the client spec.
- `lore-logs/`: the Lore log the run wrote; `lore-logs-before/`, one found at its start.
- `<run>-<time>/`: `build.sh`'s logs: `gpf.log` and `build.log` (console), `gpf-ubt.log` and
  `ubt.log` (UnrealBuildTool's own, timestamped), `steps.log` and `summary.log`.
- The sync or clone log; for the FUSE runs `cold-build.sh`'s `service-`, `clone-`, `env-` and
  `net-<run>.log`, and for `fuse-record` the recording.

### Run 1: p4

The cache drop, then `p4 sync --parallel=threads=16 //$P4CLIENT/...@$FN_CL` into the empty
workspace root, timed (`sync_s`), then `../m2/build.sh` on it: the project file generation
(`gpf_s`) and the build (`build_s`). `total_s` is their sum; `build.sh` waits for a quiet machine
before it starts timing, outside the total. The cleanup empties the
have list (`p4 flush ...#none`) before deleting the root, so the next run starts from an empty
workspace.

### Run 2: Lore, ordinary clone

The cache drop, then `LORE_USE_SERVICE=0 lore clone --revision $BENCH_REV --view $FN_VIEW
--max-connections 4` into `FN_LORE_TREE`, timed (`clone_s`), then the generation and the build,
as in run 1. The shared store a
previous run left is deleted first, so no store is warm.

### Run 3: Lore over FUSE, no prefetch, recording

`cold-build.sh` stops the service, deletes the shared store, external directories and registry,
drops the page cache, starts the service with the caller's environment, clones `BENCH_REV` into
`FN_MOUNT`, generates the project files and builds, and stops the service, which flushes the
recording and unmounts the tree. It prints
`<run> clone_s=… gpf_s=… build_s=… total_s=… exit=… KEEP|DISCARD`.

- The run clones with no recording and `LORE_TREE_PREFETCH_DEPTH=0`, which turns the tree
  prefetch off; without it the service fetches every node block in the background (`blocks`).
  With neither, every directory and file the build opens first waits one round trip: a
  ShaderCompileWorker build's makefile step alone ran past 4 minutes this way.
- The recording is `recording/<instance id>-<unix seconds>.txt` in the run directory, one path per
  first access, directories with the node blocks their listings span. The service records only
  into a directory that exists, so the run creates it, and fails when no recording comes out.

### Run 4: Lore over FUSE, prefetch from the recording

The same as run 3, cloning with the recording of the latest `fuse-record` run whose status is
`ok` (or `FN_RECORDING`), and recording nothing.

- The recording's blocks and paths are fetched while the build runs; `clone_s` is the clone alone
  (about 2 s) and `total_s` includes the build overlapping the prefetch.
- `FN_TREE_PREFETCH_DEPTH=` (set and empty) gives the configuration the milestone 2 and 3 cold
  builds measured: the recording plus the default `blocks` tree prefetch.
- The Lore log in the run directory carries each prefetch part's summary: entries, MiB and
  seconds.
- A run 4 whose status is `ok` keeps its shared store for run 5 (`kept for fuse-warm: ...`). A
  later run 4 refuses to start while that store exists: run 5 deletes it, or `clean` does.

### Run 5: Lore over FUSE, warm store, no prefetch

The same as run 3, with no recording, over the shared store run 4 kept: `cold-vfs.sh` keeps the
store (`M2_KEEP_STORE=1`), still deletes the external directories and drops the page cache, so the
fetched data is warm on local disk while the page cache, the generation and the build start
cold. `results.log` notes the store's size first (`warm store_mib=...`). The run refuses to start
without that store, and its cleanup deletes the store.

### Run 6: the build on local disk, cold

`local-build` generates the project files and builds in the ordinary clone at `FN_LORE_TREE`, at
`BENCH_REV`, after dropping the page cache: no FUSE, nothing fetched, no build output left. It
refuses to start unless `lore status --scan` reports the tree at `BENCH_REV` with no change and
no top-level `Intermediate/` or `FortniteGame/Binaries/Linux` exists, and keeps the tree, with the
build's outputs inside it. Clean the tree before another run: delete what the revision does not
hold (list the revision's files from a virtual clone with the view and compare), then
`lore reset <path>` any tracked file the build changed. Never `lore reset --force` a view-filtered
clone: forced, the reset ignores the filter and restores everything the view excludes.

## Reading the results

- `/data/fn-exp/runs/results.log`: every run's results lines, each prefixed with its run
  directory's name.
- `net-<run>.log` in a FUSE run's directory: time and received bytes each second on the interface
  to the server; the link rate the run saw. Compare runs only on a similar link.
- p4 syncs from the Stockholm proxy, while Lore fetches from us-east-2: the network favours p4.
- A run whose status is `discard`, or one on a busy machine, is not comparable. `quiet-run.sh
  <label> <command>` reruns a command until it finishes undisturbed; on a long build on a desktop
  it may restart many times, so prefer a machine nothing else uses.
