---
lep: 2026-11-11-configurable-external-directory-root
title: Configurable external directory root
authors:
  - Mattias Jansson
status: Accepted
created: 2026-11-11
updated: 2026-11-12
---

# Configurable external directory root

## Summary

A virtual clone is a repository instance whose working tree a virtual filesystem presents: FUSE on Linux, SWFS on Windows. Lore keeps each virtual clone's Lore directory, and the files that its virtual filesystem writes, in an *external directory* outside the mount. Every external directory sits in one fixed *external root*, `external/` in the global data directory. This proposal adds an `external-root` file to the global data directory, which names a different external root, and a command and C API function, `lore service set-external-directory`, that change it and move every existing external directory to the new external root. The move stops the service and occupies its socket, so no service can start until the move ends, apart from a Windows service older than v0.13.0, which can take a socket over. It holds the locks of the shared stores while it switches roots, so no Lore process of any version can read a half-moved root. A move record and one root ID per global data directory let Lore finish or undo an interrupted move without manual repair. A marker in the external root keeps Lore from this proposal on from retiring virtual clones, or writing new ones, while the root it names is missing or isn't the one the command moved to. A redirect at the old default location, a symbolic link on Linux and a directory junction on Windows, keeps older Lore versions finding moved virtual clones.

## Motivation

Lore computes a virtual clone's external directory from its instance ID. It joins `external/<instance ID>` onto the global data directory ([`external_dir.rs`](../../lore-revision/src/global/external_dir.rs)). The global data directory is the user's local data directory, or `data` below `LORE_GLOBAL_PATH` when that variable is set ([`global.rs`](../../lore-revision/src/global.rs)). Nothing else decides the location.

The external directory holds more than metadata. A FUSE virtual clone keeps every file created or changed through its mount under `write/`, beside `write-staging/` and the deletion list `deleted` ([lore-fuse README, *Writes*](../../lore-fuse/README.md)). An SWFS instance keeps its `.lore` and SWFS's `write` directory there ([SWFS through the filesystem provider](2026-10-29-swfs-through-the-filesystem-provider.md)). The overlay grows with the work done in the clone, build output included. With [virtual clones on Linux](../release-notes/virtual-clones-on-linux.md), every write through a FUSE clone lands on the volume that holds the user's data directory. If you keep large work on another volume, you can't put the overlay there.

The two workarounds both fall short:

- **`LORE_GLOBAL_PATH`** moves the global config, the shared-store registry and the default shared stores along with the external directories. Every process that uses Lore has to set it, programs that link the library included, because a call relayed to the service carries its caller's value ([`env.rs`](../../lore-base/src/env.rs)).
- **Moving the directories by hand** races with running Lore processes. A mounted FUSE clone holds files and directories in its overlay open for the life of the mount, and keeps the paths it resolved at mount time ([`fuse/write.rs`](../../lore-revision/src/fs/fuse/write.rs), [`fuse/filesystem.rs`](../../lore-revision/src/fs/fuse/filesystem.rs)).

Both a manual move and any configurable location share a worse problem. A process that looks for an external directory and finds none treats the instance as having none. Service start skips the instance without a word (`mounts_of_entries` in [`fs/mount_manager.rs`](../../lore-revision/src/fs/mount_manager.rs)). Prune reads the missing config as no virtual filesystem, judges the registration stale, and removes it along with the instance's current, branch and staged anchors (`instance_staleness`, `external_vfs` and `remove_instance_keys` in [`instance.rs`](../../lore-revision/src/instance.rs)). That happens mid-move, after a setting changes without a move, and whenever a volume that holds the external directories isn't mounted. Creating a virtual clone also creates the external directory's parents ([`repository.rs`](../../lore-revision/src/repository.rs), `create_local`), so a new clone made while that volume is missing lands on the volume underneath.

## Goals / Non-Goals

### Goals

1. Choose the external root for a global data directory with one setting, defaulting to today's location.
2. Change the setting with one command, and one C API function, that moves every existing external directory to the new external root.
3. Make sure no Lore process, of any version, uses an external directory while it moves, or reads a root that's partly moved. The move refuses rather than races. The exception, which [Risks and Assumptions](#risks-and-assumptions) accepts, is an older service on Windows during a move by copy: one on a second socket, one from before v0.13.0 that takes the socket over, or one that the command stopped and that is still closing its mounts.
4. Finish or undo a move interrupted by a failure, a crash or a power loss at any step, without manual repair.
5. Never retire a virtual clone, or write a new one to the wrong place, because the external root is missing or isn't the one Lore moved to. This holds for Lore from this proposal on; an older version on the same machine still retires virtual clones while the redirect leads nowhere (see [Risks and Assumptions](#risks-and-assumptions)).
6. Keep older Lore versions finding moved virtual clones while the external root is available and no move is interrupted.
7. Keep commands in ordinary instances running during a move. At most, one that opens a shared store with a virtual clone registered waits for the final switch, which takes seconds.

### Non-Goals

- Moving the global data directory, the global config or shared stores. `LORE_GLOBAL_PATH` still covers those.
- A location per instance. Every external directory in a global data directory sits in the one external root.
- Moving external directories while their virtual clones stay mounted. See [Move inside the running service](#move-inside-the-running-service).
- External directories that belong to another global data directory, such as one that a different `LORE_GLOBAL_PATH` names. Each global data directory has its own setting, root ID and move.
- Restoring a registration that a prune has already retired.
- macOS, which has no virtual clones: there the command changes the setting and has nothing to move.

## Proposed Design

The design has seven parts: the setting (Goal 1), the root ID and marker (Goal 5), access to the external root (Goals 3 and 7), the command and its exclusion of other processes (Goals 2, 3 and 7), the move and its recovery (Goal 4), the redirect at the old location (Goal 6), and how each consumer reads an external directory (Goals 5 and 7). Each part states what Lore guarantees, what it refuses and what holds after a failure. The [appendix](#appendix-implementation-requirements) after the last section carries the implementation requirements that keep these guarantees, and the current code they rely on, into the specification and plan.

### The setting

The global data directory gains an `external-root` file, a TOML file that holds the *setting*, the external root's absolute path, together with the root ID and generation (see [The root ID and marker](#the-root-id-and-marker)). Without the file, or with an empty path, the external root is the default, `external/` in the global data directory. A virtual clone's external directory is `<external root>/<instance ID>/`, and its layout below that is unchanged.

```toml
path = "/mnt/work/lore-external"
id = "<root ID>"
generation = 1
```

The file is the only thing Lore reads to find the external root, and Lore uses the path it names, never a path through the redirect. Only Lore from this proposal on reads or writes it, so no older version can drop the setting. The setting carries logic that `config.toml` settings don't: it changes together with the root ID and the generation, and recovery reads them together, so every change replaces the whole file, and a crash can't leave the three disagreeing. The command is the supported way to change the setting. A hand edit that names a directory without the root's marker reads as unknown, and one that names a copy of the root, such as a backup, takes effect but leaves the redirect pointing at the old root; `--adopt` switches to such a copy and makes the redirect match.

### The root ID and marker

Each global data directory has one *root ID*, a random value that the command creates the first time it moves the external root and that never changes, and a *root generation*, a number that starts at 1 and rises only when `--abandon` gives up a root. The command stores both in `external-root`, beside the setting. Every external root the command has moved to holds a marker file with the root ID and the generation it belongs to. The marker moves with the root's contents, so after a move the new root holds it, and a move keeps the generation.

Lore treats an external root as *available* by these rules:

- With no root ID stored, only the default root is available, and it needs no marker, as long as it's a directory and not a redirect. That's an installation that has never moved its external root, which is how Lore works today. A redirect at the default location without `external-root` means the file was lost, and reads as unknown until `--adopt` writes it again.
- With a root ID stored, a root is available only when it holds a marker with that root ID and the current generation, and, on Linux, you own the root and the marker and no one else can write them.

An external root that isn't available, because its volume isn't mounted, the setting names a directory the command didn't move to, `external-root` was copied from another machine, or `external-root` was lost, reads as *unknown* (see [Reading an external directory](#reading-an-external-directory)). A second global data directory that names the same root finds a marker with an ID that isn't its own, so two global data directories can't share a root. A root that `--abandon` gave up keeps a marker from an earlier generation, so it stays unavailable if its volume comes back, whatever the setting names. On Linux, the command creates `external-root` and the marker with access for you only, and gives every root it moves to access for you only, whether it renames or copies.

### Access to the external root

Only services and the command take access to an external root, through an operating-system file lock on `external.lock` in the global data directory. Every write to an external directory already happens in the Lore service. Other processes only read external directories, either through a live FUSE mount, which the command refuses before a move and again before the final switch, or under a shared store's lock, which the command holds while the roots change (see [The final switch](#the-final-switch)), so their reads take no access. [Where external directories are used](#where-external-directories-are-used) cites the code for each.

- **Shared access.** A service that mounts a virtual clone from an external root, or creates, deletes or moves one there, holds shared access to that root until its process exits, since paths into an external root outlive both the mount and the command that used them. A service never waits for shared access: while the command holds exclusive access, a call that needs shared access fails with an error that names the move, and a mount at service start is skipped and reported.
- **Exclusive access.** The command holds exclusive access for the whole move, and recovery holds it while it runs. It takes exclusive access only once no service holds shared access.

Access to the external root adds no lock order to the one that [ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md) fixes: no holder of shared access ever waits, and the holder of exclusive access takes no repository lock and waits for nothing but the shared-store locks.

### The command

`lore service set-external-directory <path>` and `lore_service_set_external_directory` change the setting and move every external directory from the current external root to `<path>`. A relative path resolves against the caller's working directory, as `lore repository instance move` resolves its target. An empty path means the default location, `external/` in the global data directory, as a physical directory: moving back there replaces the redirect with the root itself. The command runs in the calling process and is never relayed to a service.

Two further forms change the setting without moving anything. Both are recorded and recovered like a move, and both make the redirect at the old location match the new setting.

- `--adopt` points the setting at a directory that already holds this global data directory's marker from the current generation, such as a root moved by hand or restored from a backup of the current root. It refuses a directory whose marker is missing, holds another root ID, or is from an earlier generation, so it can't bring back an abandoned root. When `external-root` is missing, there's no root ID or generation to check against, so `--adopt` trusts the marker of the directory you name, takes its root ID and generation, and writes the file again, which recovers a lost file.
- `--abandon` gives up a current root that's unavailable for good, such as one on a failed volume. It switches to a new, empty root and raises the generation, so the old root can't become available again if its volume returns. The virtual clones that lived in the old root become absent, and prune retires them. It refuses while the current root is available, and while `external-root` is missing, since there's no root ID to keep; `--adopt` writes the file again first.

**Refusals.** The command checks the target, and that it can stop the service safely, before it stops anything, and in a dry run. It refuses, with `InvalidArguments` for a target, with `MountsBusy` naming what's in use for a service or mount, and as an internal error with a message that names the cause otherwise, when:

- the target exists, other than the default location holding the command's own redirect and, with `--adopt`, the directory it names; the command creates any other target and its missing parents itself;
- the target lies inside the current external root, or contains it;
- the target lies inside a repository instance's working tree, or inside the path of any registered virtual clone, mounted or not;
- on Linux, the target's parent is a directory that other users can write;
- on Linux, the target's file system isn't one the system reports as local, compares names differently from the current root's, or can't hold symbolic links, permission bits and modification times to the nanosecond;
- the move would copy (see [The move](#the-move)) and the target's volume has less free space than the external directories occupy;
- on Windows, the target isn't on a local volume, which a directory junction requires: the command checks the volume the path resolves to, so a network drive, a UNC path, or a drive letter mapped to a network share is refused;
- on Windows, the move would copy and the target's file system can't hold access control lists, alternate data streams and sparse files;
- on Windows, the caller's working directory is inside the current root, which Windows refuses to rename;
- the running service runs inside another program through the C API, or has virtual clones of another global data directory mounted, since stopping it would take down work the command can't restart; a service from before this proposal can't report either, so the command stops it without this check;
- on Windows, the executable that serves after the move reports a version from before this proposal, or can't report one, since its SWFS can't read a write directory through the redirect;
- a move is recorded as interrupted, until recovery has run (see [Recovery](#recovery)), except that `--abandon`, and `--adopt` of a root that holds a marker from the current generation, can take over a move that recovery waits on because a root isn't available. A takeover by `--adopt` gives up the move's other root, which Lore removes as a leftover, since nothing mounts from either root while the move is recorded. `--abandon` refuses to take over while the move's old root is available, so that you adopt it instead.

The default location is exempt from the file-system checks, since Lore uses it today wherever the global data directory is. A target equal to the current external root succeeds with nothing to move.

Once the service is down and the command holds exclusive access, it checks the target again, and refuses while, on Linux, the mount table lists a live `fuse.lore` mount of an instance in this global data directory, which catches a service of any version on another socket.

**Stopping and occupying the service.** If a service is running, the command stops it the way `lore service stop` does: it refuses while a mount is busy, and with `--force` it detaches busy FUSE mounts lazily, after which programs with files open through them get errors. The move starts only once the stopped service's process has exited, which ends its deferred work and releases its shared access, and the command refuses if that takes longer than a bounded time. A service from before this proposal reports no process ID, so the command stops it as best effort, and on Windows can't tell that its SWFS mounts have closed (see [Risks and Assumptions](#risks-and-assumptions)).

From the stop to the end of the move, the command occupies the service socket and answers every call with an error that names the move, and it refuses if a service claims the socket before it does. While it holds the socket, no service on Linux or macOS, and no Windows service from v0.13.0 on, can start on that socket. A Windows service from before v0.13.0 takes a socket over ([release notes](../release-notes.md), v0.13.0), which [Risks and Assumptions](#risks-and-assumptions) covers. A current client that gets this answer runs a command in an ordinary instance in its own process, and fails a command in a virtual clone with the error. An older client fails the call.

After the move, the command starts a service again from the executable the stopped one ran, if it stopped one from this proposal on. After it stops an older service, which can't report its executable, it starts none. If that start fails, the move still stands, the command reports the failure, and the next command that needs the service starts one.

A dry run runs every check that needs nothing stopped, reports a running service as one it would stop, reports how many external directories would move, their total size, and whether the move would rename or copy, and reports any interrupted move. For a copy, it says that each FUSE virtual clone reads and hashes the files in its overlay again afterwards (see [Non-Functional Considerations](#non-functional-considerations)). It stops nothing and changes nothing. The command reports each external directory as it copies it, a summary at the end, and whether it started a service again.

### The move

The move holds these properties:

- **Rename on one volume, copy across volumes.** On one volume, the move is one rename of the root. If the rename fails, after retries while the system reports files in use, the move rolls back and refuses. It never falls back to a copy.
- **A faithful copy.** A copy keeps file contents, the holes of sparse files, and modification times. On Linux it also keeps permission bits, ownership, times to the nanosecond, extended attributes including access control lists, and symbolic links as links, and refuses a hard link. On Windows it also keeps file attributes, access control lists and alternate data streams, and refuses a reparse point and a file with more than one hard link. It refuses any other kind of entry, and anything the target can't hold, rather than copy it differently. It copies everything in the root except the contents of `write-staging/`, which FUSE empties at mount, and nothing becomes the target until the whole copy is on disk.
- **Recorded.** Before its first change to any root, the command writes a move record in the global data directory. The record names the old and new roots, the kind of move, and how far the move has got.
- **The switch decides the move.** The command switches the setting to the new root, on disk, before it removes anything of the old one. A crash before the switch undoes the move; a crash after it finishes the move.
- **Nothing complete is removed.** Lore never removes the external root the setting names, and removes the old root only once the new one holds the marker and the setting names it. Before Lore sets aside what's left of an old root, or a copy that an undone move made, it removes that directory's marker, so the set-aside directory can't read as a root.
- **Removal doesn't block.** Lore lists what's left of the old root as a *leftover*, and removes it in the background once the move ends. A removal that doesn't finish is retried by the next run of the command and by service start, and never blocks a virtual clone.
- **Durable.** Every write the move depends on is on disk before the step that relies on it, so a power loss at any point leaves at least one complete external root that the record and the setting identify.

### The final switch

Every step that changes which root holds the data or what the default location holds runs while the command holds the locks of every shared store with a virtual clone registered: placing the new root, switching the setting, setting aside what's left of the old root, and changing the redirect. These steps make up the *final switch*. Every Lore process holds a shared store's lock for as long as it uses the store ([`store_lock.rs`](../../lore-storage/src/local/store_lock.rs), [ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)), and listing, pruning and mounting virtual clones all read their registrations from those stores. So no process of any version can look for an external directory while the roots change, and the command never reaches the old root through a path that has become a redirect. If a live `fuse.lore` mount of this global data directory has appeared on Linux by the time the command takes the locks, the command rolls the move back.

Taking those locks can't deadlock with a program that holds one of those stores while it waits for another, and a stream of short commands doesn't starve it. While a store stays in use, the command names it and keeps a finished copy, and you can cancel from the CLI, which undoes the move. Through the C API, which can't cancel a single call ([`lore.h`](../../lore-capi/lore.h)), the call returns only once the command holds every lock. Before a copy starts, the command checks that no store is held open, so you see one before the copy rather than after it. A command in an ordinary instance that opens one of those stores in the meantime waits at most a few seconds while the command takes the locks, and then for the final switch, which takes seconds.

### Recovery

Lore finishes or undoes an interrupted move without manual repair. Service start runs recovery before it mounts anything, and so does the command before it starts a new move. Recovery holds exclusive access, and holds the shared-store locks for the steps of the final switch, as a move does. A store held open never keeps a service from accepting calls: the service leaves the move waiting, retries recovery in the background until it finishes, and then mounts the root's virtual clones. Service start never waits for an old root's removal, which runs in the background.

The move record, the setting, and which roots are available decide which way an interrupted move goes:

| Setting names | Available root | Recovery |
|---|---|---|
| the old root | the old root, and the new root too once a copy is placed | Undo: keep the old root, and set aside what the move made at the target. |
| the old root | the new root only, after a rename | Finish: switch the setting to the new root, then continue as in the next row. |
| the new root | the new root | Finish: keep the new root, and remove what's left of the old one as a leftover, once its volume is there. |
| the old or the new root | none of the rows above, such as a root on a volume that isn't mounted yet | Wait: change nothing and keep the record. The virtual clones read as unknown, and the next service start runs recovery again, until the root is available or `--abandon` or `--adopt` takes the move over. |
| neither root | either | Refuse, report both roots, and change nothing. |

Undo and Finish leave the redirect matching the setting, and end the move. A first move on an installation with no default root yet, which creates the new root rather than moving one, counts the missing default as an available, empty old root until it stores the root ID, so it never waits. The last row is a state no crash produces, such as a hand edit during an interrupted move. A failure without a crash follows the same table at once. Finishing a rename forward, rather than renaming the root back, avoids a second rename that Windows can refuse while files are in use. An interrupted `--abandon` or `--adopt` is always finished, never undone: the root that `--abandon` gives up is unavailable, and `--adopt` changes nothing in either root.

### The redirect at the old location

The command leaves a *redirect* at the old default location, `external/` in the global data directory, pointing at the external root: a symbolic link on Linux, and a directory junction on Windows. A symbolic link on Windows needs the *Create symbolic links* user right, which only administrators hold by default ([Create symbolic links](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/create-symbolic-links)), and a junction is taken to need none (see [Risks and Assumptions](#risks-and-assumptions)). A junction can only point at a directory on a local volume ([Hard links and junctions](https://learn.microsoft.com/en-us/windows/win32/fileio/hard-links-and-junctions)), which validation already requires.

- The command makes the redirect after a move away from the default location, points it at the new root after a move between two configured roots, and, in a move back to the default location, removes it during the final switch, just before the root takes its place. `--adopt` and `--abandon` make it match the new setting the same way. The move record covers each change, so recovery leaves the redirect matching the setting, including a junction the command made but hadn't finished.
- It only replaces a free path or a redirect it made. When the default location holds a directory with entries while the setting names another root, the command and service start report it, and the command makes no redirect over it.
- Lore from this proposal on reads the external root's path from `external-root` and checks the marker. It never reaches the root through the redirect, which only older versions follow.
- Through the redirect, an older Lore on Linux reaches the external directories of moved virtual clones as it reaches them at the default location today. While the redirect leads nowhere, it retires them (see [Risks and Assumptions](#risks-and-assumptions)).
- On Windows, Lore from this proposal on hands the SWFS driver the write directory's path in the external root itself. A Lore version from before this proposal finds moved virtual clones through the junction, but hands its SWFS a write directory whose path passes through the junction, which its SWFS can't mount. One SWFS mount that fails stops such a service's whole start (`mount_registered` in [`fs/mount_manager.rs`](../../lore-revision/src/fs/mount_manager.rs)), so every command it would serve fails, ordinary instances included. That's why the command refuses when such a version would serve after the move.

### Reading an external directory

A lookup of an instance's external directory has three outcomes: present, absent, and unknown. It's unknown while a move is recorded, when the external root isn't available, and when the instance's external directory is missing from the configured root but present in a directory at the default location. Each consumer treats unknown as follows:

| Consumer | On unknown |
|---|---|
| Prune, listing, and the multiple-instance warnings in `lore sync` and `lore branch switch` | Keep the registration: prune doesn't retire it, and listing and the warnings count it as active. |
| Service start and mounting | Skip the virtual clone and report the root's state. A later service start mounts it once the root is available. |
| Creating a virtual clone, or cloning or creating at a registered virtual clone's path | Refuse, naming the root's state. |
| Deleting or moving a virtual clone | Refuse, naming the root's state. |
| Discovery of a virtual clone from a path | Fail, naming the root's state. |

Mounting and creating a virtual clone check that the root is available each time, and never create the root itself, so a root that goes away while a service runs makes the mount or the creation fail instead of recreating the root on the volume underneath.

Cloning or creating a repository at the path of a registered virtual clone refuses whenever that clone's external directory is present or unknown. Today, registering a new instance at an empty mount point retires the clone there (`register_instance` in [`instance.rs`](../../lore-revision/src/instance.rs)), and an unavailable root makes empty mount points last longer.

`lore service status` reports the external root and its state: available, unavailable, interrupted, or with leftovers. It also reports whether the service runs inside another program, the global data directories it has virtual clones mounted for, and its process ID, which the command needs before it stops a service. A service from before this proposal reports none of these, and the command stops it as best effort (see [The command](#the-command)).

## Compatibility

- **Wire format** — N/A. Nothing changes between a client and Lore Server.
- **Client/server protocols** — N/A for Lore Server. The service's IPC gains one reply, the error that names a move, which the command sends from the occupied socket. The reply goes after the existing ones so their encoded indices don't move ([ADR-00022](../developing/decisions/00022-service-ipc-bitcode.md)). A client from before this proposal can't decode it and fails the call. The event that `lore service status` emits gains fields too (see CLI and public API), and since bitcode has no field-level compatibility, a client and a service from different builds can't read each other's events, as with any change to the IPC ([ADR-00022](../developing/decisions/00022-service-ipc-bitcode.md)). The command itself is never relayed.
- **On-disk format** —
  - The global data directory gains `external-root` (the setting, a TOML file with `path`, `id` and `generation`), `external.lock` (access to the external root), `external.leftovers` (old roots still to remove), and, during or after an interrupted move, `external.move` (the move record, a TOML file with `from`, `to`, `kind`, which is `rename`, `copy`, `create`, `adopt` or `abandon`, and `phase`). `config.toml` doesn't change.
  - Each external root the command moves to holds a `.lore-external-root` marker file with the root ID and the generation it belongs to.
  - `external/` in the global data directory can be a redirect to the external root: a symbolic link on Linux, a directory junction on Windows.
  - A Lore version from before this proposal ignores all of these. The layout inside each external directory, the instance's Lore directory and its stores don't change, and nothing in them records a path into the external directory.
- **CLI and public API** —
  - New: the `lore service set-external-directory <path>` subcommand, with `--adopt` and `--abandon`. It honors the global `--dry-run` and `--force`; `--force` only detaches busy FUSE mounts lazily, as it does for `lore service stop`.
  - New: `lore_service_set_external_directory` and `lore_service_set_external_directory_async` in `lore-capi`, with the same signature as `lore_service_set_executable`, taking `lore_service_set_external_directory_args_t` with a `lore_string_t path` and Booleans `adopt` and `abandon`. They emit the standard events and no new event type. A program that runs the service through the C API is refused, which the function's description says.
  - Changed: the event that `lore service status` emits gains, after its existing fields, the external root, its state, whether the service runs inside another program, the global data directories with virtual clones mounted, and the process ID.
  - Exit codes: an invalid target exits with `InvalidArguments` (3). A refusal because something is in use, a busy mount, a service the command can't stop safely, a live Lore mount, a shared store, or a claimed service socket, exits with `MountsBusy` (51), with its `paths` naming what's in use. An executable too old to read through the redirect, an interrupted move, or an unavailable external root fails as an internal error with a message that names it. Lore adds no error codes.
  - Changed behavior of existing commands:
    - During a move, with `use_automatically`, a command in an ordinary instance runs in the caller's process, and a command in a virtual clone fails with the error that names the move.
    - Service start runs recovery before it mounts, and skips and reports the virtual clones of an unavailable root.
    - Prune and listing report virtual clones whose external directory is unknown as active.
    - `lore clone`, `lore repository create` and `lore repository instance clone` refuse a destination that's a registered virtual clone's path while its external directory is present or unknown.
    - Creating, deleting and moving a virtual clone refuse while its root is unavailable or a move is recorded.
- **Documented location** — The [lore-fuse README](../../lore-fuse/README.md) places external directories at `<global data>/external/<instance id>/`, and the CLI reference for instance prune ([`lore-cli-commands.md`](../reference/lore-cli-commands.md)) keeps an SWFS instance "while its `.lore` remains in the global data directory". Both change to the external root.
- **Instance move and delete** — The accepted [Instance move and delete](2026-10-19-instance-move-and-delete.md) proposal places an SWFS instance's rename-aside sibling beside its external directory, which it describes as in the global data directory. The sibling stays beside the external directory, now in the external root. That wording goes stale, which an amendment to that proposal corrects; its design doesn't change.
- **Copy-on-write instance clone** — The draft [Copy-on-write instance clone](2026-10-31-copy-on-write-instance-clone.md) proposal clones an SWFS instance's external directory with the backend for its volume. It runs in the service, which holds shared access, and its cost now depends on the external root's volume.

## Non-Functional Considerations

- **Concurrency** —
  - **Another run of the command** can't take exclusive access, or the socket, while the first holds them.
  - **The service** is stopped before the move starts and, if it's from this proposal on, has exited. No service can start on its socket until the move ends, apart from a Windows service from before v0.13.0, which can take it over (see [Risks and Assumptions](#risks-and-assumptions)).
  - **A service on another socket** of this proposal's version can't take shared access during a move, so its calls that need shared access fail with the move's error. One of any version that has virtual clones of this global data directory mounted shows in the Linux mount table, which makes the command refuse. On Windows, its SWFS mounts hold handles in the root, which make a move by rename fail and roll back; a move by copy reads around them (see [Risks and Assumptions](#risks-and-assumptions)).
  - **Commands in virtual clones** fail with the move's error.
  - **Commands in ordinary instances** run in their own process while the service socket is occupied. One that opens a shared store with a virtual clone registered waits for the final switch.
  - **Prune, listing and mounting**, in any version, can't run while the command or recovery changes the roots, because they hold a shared store's lock and the command and recovery hold them all. After an interrupted move, until recovery runs, an older version can find the default location missing or its redirect leading nowhere (see [Risks and Assumptions](#risks-and-assumptions)).
- **Durability** — The marker, `external-root`, the move record, a placed copy and the redirect are each on disk before the step that relies on them, by flushing the file and its directory on Linux, and by write-through renames on Windows. A power loss at any point leaves at least one complete external root that the record and the setting identify.
- **Memory** — The copy streams each file through a fixed-size buffer and walks one directory at a time, so its memory doesn't grow with the size of an overlay or the number of files. The move record holds two paths, a kind and a phase.
- **Statelessness** — A service holds shared access, for the life of the process, to each external root it has mounted from or created, deleted or moved a virtual clone in. `external-root` and the markers are persistent state that changes only when the command creates them, moves the root, or adopts or abandons one. A move record survives only an interrupted move, and recovery removes it. The leftover list empties as Lore removes the leftovers.
- **Determinism** — Preserved. The setting decides where files live, not what any command computes or records.
- **Availability** — While the external root is unavailable, its virtual clones are unavailable too, and come back at the next service start after it returns. Nothing is retired in between. During a move, virtual clones are unavailable, and ordinary instances aren't.
- **Performance** — A command in an ordinary instance pays nothing while no move runs: a client checks for a move only when the service answers with the move's error. A lookup of an external directory adds a read of `external-root` and of the marker, and one check for a move record. Listing, prune, the multiple-instance warnings and service start look one up, and so does a command in a FUSE virtual clone, both in the client, which decides where the command runs (`instance_requires_service_given` in [`call_delegation.rs`](../../lore/src/call_delegation.rs)), and in the service. FUSE requests don't touch any of this. A move by rename takes constant time. A move by copy takes time proportional to the size of the external directories. A copy can't keep the time each file's status last changed, which FUSE records and compares to find changed files ([`fuse/operation.rs`](../../lore-revision/src/fs/fuse/operation.rs)), so after a copy each FUSE virtual clone reads and hashes the files in its overlay again at its next scan or commit.

## Migration Plan

Nothing changes for a user who doesn't set the setting, except one who linked `external/` elsewhere by hand: with no `external-root`, that link reads as unknown (see [The root ID and marker](#the-root-id-and-marker)). After a move, Lore versions on one machine relate as follows:

| Reader | Linux | Windows |
|---|---|---|
| This version and later | Reads `external-root` and checks the marker | Same |
| A version from before this proposal | Reaches the external directories through the redirect, as it reaches the default location today, and retires virtual clones while the redirect leads nowhere | Finds clones through the junction, but its SWFS can't mount them, and the failed mounts stop its whole service start |

Older versions take no access. Apart from a Windows service from before v0.13.0, they can't start a service on the occupied socket, and they hold a shared store's lock whenever they list, prune or mount, so they can't reach a root while the command or recovery changes it. After an interrupted move until recovery runs, and while the external root's volume isn't mounted, they can find no external directories and retire the virtual clones (see [Risks and Assumptions](#risks-and-assumptions)).

**Rollback.** Before you downgrade, or run a Lore version from before this proposal as the service on Windows, run `lore service set-external-directory ""` to move the external directories back to the default location. The signal that you skipped this is an older Lore reporting that a virtual clone's path isn't a repository, or an older Windows service that fails to start, since the SWFS mounts it can't make stop its whole start. The external directories and their overlays stay on disk, and moving back recovers them. A registration that an older prune retired in the meantime isn't recovered.

## Security Considerations

The trust model doesn't change. The setting, the root ID and the generation live in `external-root` in the user's own global data directory, which only that user, or an administrator, can write, unless `LORE_GLOBAL_PATH` names a directory that others can write. The command runs as the user and moves only that user's external directories. A repository or a remote can't set or influence the setting.

The overlay holds the same user content as an on-disk working tree, and a planted external directory could make the service mount a Lore directory that names someone else's store. So the command creates the target itself and refuses one that exists, so no one else can plant it. On Linux, it also refuses a parent that other users can write, so no one else can swap the target or the temporary directory beside it, and the target, the temporary directory, `external-root` and the marker get access for the user only, whether the move renames or copies. On Windows, a target that the command creates inherits its parent's access, and the command doesn't check the parent: keeping the target where other users can't change it is up to you, as it is for any directory that holds your work. A copy keeps the permission bits, ownership and access control lists of what it copies, and a rename keeps them as they were.

A directory that someone else creates at the configured path isn't used unless it holds a marker with the root ID, which only the user's global data directory and the user's external root hold, and, on Linux, unless the user owns it and its marker and no one else can write them.

## Privacy Considerations

No new data leaves the machine. The external root's path appears in `external-root` in the user's own global data directory, in the move record, in `lore service status`, in the command's output and in local logs, as other paths in the global data directory already do. The root ID is a random value that never leaves the machine. Nothing is sent to a server or to telemetry.

Deleting an instance still removes its external directory, now from the configured external root. After a copy, the command removes the old copy. Like instance delete, that unlinks the files and doesn't overwrite them, so their blocks can stay on the old volume until the file system reuses them.

## Risks and Assumptions

**Assumptions**

- **Assumption:** Nothing in an external directory records a path into the external directory, or a property of its files that a copy changes and that decides what Lore reads as a file's content; FUSE compares status-change times, which only make it read a file again (see [Non-Functional Considerations](#non-functional-considerations)). The FUSE overlay holds repository-relative paths, and a virtual clone's Lore directory holds its ID and instance files, config, lock file, view, ignore rules, layer config and modified-time records; its config names its shared store's path, which lies outside. — *invalidated if:* a store, config, record file or the FUSE overlay is found to hold an absolute path into the external directory, or a property that a copy changes is found to change what Lore reads.
- **Assumption:** Every Lore process that uses a shared store holds the store's lock for as long as it uses it, in every version that can have a virtual clone registered ([ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)). — *invalidated if:* a version that supports virtual clones reads a shared store's registrations without its lock.
- **Assumption:** Windows refuses to rename a directory while a process holds a handle open on a file or directory inside it, so a rename move can't race an SWFS mount that a crashed service left behind. — *invalidated if:* such a rename succeeds, in which case the command has to detect live SWFS mounts in another way.
- **Assumption:** A user without administrator rights can create a directory junction on Windows. — *invalidated if:* creating one needs a user right that users don't hold by default; then the command can't make the redirect for them, and older versions on Windows don't find moved virtual clones.
- **Assumption:** The SWFS driver keeps nothing in its write directory that depends on the directory's path or volume, and accepts a write directory renamed within its volume or copied to another volume as ordinary files and directories. — *invalidated if:* SWFS records absolute paths, file IDs or the volume in its write directory, or refuses a renamed or copied one; then moves on Windows can't keep SWFS instances working, and a move across volumes has to refuse while SWFS instances exist.
- **Assumption:** Every write to an external directory runs in the service, and every path to one is computed from the setting in one place ([`external_dir.rs`](../../lore-revision/src/global/external_dir.rs)). — *invalidated if:* a client writes an external directory, or a code path builds an external directory's path some other way.
- **Assumption:** On Windows, `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` returns only once a rename within a volume is on disk, which Microsoft documents for a move done as a copy and delete. — *invalidated if:* such a rename is lost on a power loss after the call returns; then Lore needs another way to make a rename durable on Windows.

**Risks**

- **Risk:** A copy of large overlays takes long, and the virtual clones are unavailable for that time. — *mitigation:* a dry run reports the size and whether the move renames or copies. A move on one volume is a single rename.
- **Risk:** The target's volume fills during the copy, despite the free-space check, because another program writes to it. — *mitigation:* the copy fails before the switch, and recovery removes the temporary directory.
- **Risk:** A long-running program holds a shared store in use, so the final switch can't take its lock. — *mitigation:* the command keeps a finished copy, names the store, and starts over until it can take every lock, releasing the others in between so that other commands aren't held up. You close the program, or cancel from the CLI. It checks the stores before a copy starts, so a store held open shows before the copy rather than after it.
- **Risk:** On Windows, where there's no mount table to check, a service of an older version writes into the old root while a move by copy runs, and what it writes after its files are copied is lost. That happens with a service on a socket other than the configured one; with one from before v0.13.0 that takes the command's socket over, after which the command, when it closes its listener, also removes that service's socket file ([`local_socket.rs`](../../lore-transport/src/local_socket.rs)); and with an older service that the command stopped, which reports no process ID to wait on, while it still closes its SWFS mounts. — *mitigation:* a move by rename fails while those mounts hold handles in the root, and rolls back. A move by copy relies on no older service running: the command's documentation says to stop older services, and close programs that host one, before a move. Later mounts of such a service need a shared store's lock, which the command holds during the switch.
- **Risk:** An older Lore on the same machine retires virtual clones while the redirect leads nowhere: while the external root's volume isn't mounted, and after an interrupted move until recovery runs. Its prune reads the missing config as no virtual filesystem (`load` in [`util/config.rs`](../../lore-revision/src/util/config.rs), `instance_staleness` in [`instance.rs`](../../lore-revision/src/instance.rs)), and an older `lore clone` on a shared store runs that prune in the background ([`repository/clone.rs`](../../lore-revision/src/repository/clone.rs)). — *mitigation:* while older versions are in use, keep the external root on a volume that's always mounted; `lore service status` and the command report an unavailable root, and the command's documentation says so. Lore from this proposal on never retires them.
- **Risk:** On Windows, a target in a location that other users can change, such as a folder at the root of a volume, lets them replace it, or read its marker and plant a root that holds a copy. — *mitigation:* the target inherits its parent's access, so a location in your user profile, or one whose access you restrict, keeps others out. The command's documentation says so.
- **Risk:** The service start after the move fails. — *mitigation:* the move stands, the command reports the failure, and the next command that needs the service starts one.

## Drawbacks

- A move stops the service, which takes down every virtual clone in the global data directory and relays nothing until the move ends.
- A move by copy needs free space for a second copy of every external directory until the old one is removed.
- The global data directory gains four small files, and a redirect at the old default location.
- On Windows, the command doesn't check who else can change the target's location.

## Alternatives Considered

### Leave the location fixed, and document a symbolic link or `LORE_GLOBAL_PATH`

You would move the directories by hand and point a symbolic link at the new location, or set `LORE_GLOBAL_PATH` in every process.

*Rejected because:* nothing keeps a running Lore process away from the directories while they move, and nothing stops prune from retiring registrations while they're missing, which is the failure described in [Motivation](#motivation). Nothing records a move, so a crash halfway leaves you to repair it by hand. `LORE_GLOBAL_PATH` also moves the global config and shared stores, and has to reach every program that links Lore.

### Name the location with the redirect alone

The command would move the external directories and leave only the redirect at the default location, with no setting.

*Rejected because:* a redirect that someone deletes, or a backup or file-synchronization tool that doesn't restore it, would lose the location, and on Windows every SWFS mount would depend on the driver reading through the junction, which it can't. `external-root` names the real path, and you can read it back. The redirect stays as a helper for versions that don't read `external-root`.

### Keep the setting in `config.toml`

The `[service]` table of the user-level `config.toml` would gain `external_directory`, beside `executable`, `use_automatically` and `idle_connection_timeout`, and the root ID and generation would sit in a file of their own.

*Rejected because:* the setting isn't an ordinary setting. It changes together with the root ID and the generation, and recovery reads all three together, so two files would let a crash leave them disagreeing. Lore versions from before keeping unknown keys ([release note](../release-notes/global-config-keeps-unknown-keys.md)) drop the setting when they rewrite `config.toml`, and after an older version on the same machine dropped it, Lore would read the default location, which is the redirect, as the external root: moving back would do nothing, and a later move would act on the redirect. Only Lore from this proposal on writes `external-root`.

### Record each instance's external directory where it's registered

Each instance would record its external directory's path at clone time, and Lore would read it from the record rather than compute it from the setting.

*Rejected because:* discovery finds a FUSE virtual clone from a path with only the instance ID that the mount carries, before it has opened any store ([`fuse/discover.rs`](../../lore-revision/src/fs/fuse/discover.rs)). A record would add a field to every instance registration in every shared store's mutable store, which is an on-disk format change, and [ADR-00018](../developing/decisions/00018-external-instance-directory-registry.md) chose the shared-store registry so that external directories need no registry of their own. Moving existing instances would still need the same exclusion and recovery as this proposal.

### Make services wait on a pending-move lock

The command would hold a lock that a starting service waits on before it mounts, instead of occupying the socket.

*Rejected because:* a service that waits before it listens makes every relayed command, ordinary instances included, time out after ten seconds ([`service_process.rs`](../../lore/src/remote/service_process.rs)), and the command's own restart deadlocks on the lock it holds. A service from before this proposal ignores the lock and mounts the old root mid-copy. Occupying the socket keeps every version out and gives current clients a clear answer.

### Bracket client lookups instead of holding the store locks

Each client would read the setting, the root ID and the move record before and after a lookup, and read any change as unknown.

*Rejected because:* it covers only clients from this proposal on, so an older prune could still retire registrations while the roots change. Every process that reads registrations already holds the shared store's lock, so holding those locks for the seconds of the final switch excludes every version with no new rule for clients.

### Take shared access in every process, for each use

Every process that reads an external directory would hold shared access while it does, and the move would wait for them.

*Rejected because:* paths into an external root outlive a command and the mount table entry, so tying access to each use leaves gaps, and every caller would have to hold a guard. Clients would also wait for a whole move.

### Move inside the running service

The command would run in the service. The service would unmount each virtual clone, move the root, and mount each clone again at the same mount point from the new root, as `lore repository instance move` mounts a clone again at a new path. Ordinary instances would keep relaying through the service, and the refusals for a service that runs inside another program or serves other global data directories would go away.

*Rejected because:* the parts it needs don't exist yet. A FUSE mount keeps the external directory it resolved, and mounting it again reuses that directory ([`fuse/mount.rs`](../../lore-revision/src/fs/fuse/mount.rs)), so a remount from a new root needs a mount that takes a new one. A deferred request still running after an unmount keeps writing to the old root until it ends ([lore-fuse README](../../lore-fuse/README.md)), and FUSE has no gate that drains it the way SWFS's handle gate does ([`fs/swfs/handle_gate.rs`](../../lore-revision/src/fs/swfs/handle_gate.rs)). The service would still need the rest of this proposal: other services and older versions kept out, the shared-store locks for the switch, the record and recovery. A later proposal can move inside the service behind the same command, setting, record and recovery, once a remount can take a new external directory and FUSE can drain deferred requests.

### Change the setting without moving anything

The setting would apply to virtual clones created after the change, and you would move existing ones yourself or keep them where they are.

*Rejected because:* Lore computes an external directory's path from the setting, so every existing virtual clone disappears from Lore's view at the change. `--adopt` and `--abandon` cover the cases where nothing should move.

## Prior Art

- **Git's separate repository directory.** `git init --separate-git-dir` puts the repository outside the working tree and leaves a `.git` file there naming its location; run on an existing repository, it moves the repository directory to the new path ([git-init](https://git-scm.com/docs/git-init)). Lore borrows the pointer at the old place, for tools that look there, as the redirect. Git's documentation says nothing about other Git processes running during that move, which this proposal handles.
- **Git worktrees on removable volumes.** `git worktree lock` exists so that a worktree on a portable device or network share that isn't always mounted keeps its administrative files when prune runs, `git worktree move` refuses a locked worktree, and `git worktree repair` fixes the links after a worktree moves by hand ([git-worktree](https://git-scm.com/docs/git-worktree)). The root marker solves the same problem without a manual step: an unavailable external root reads as unknown, and nothing is pruned. `--adopt` is the counterpart of `repair`.

## Unresolved Questions

None. Follow-up work:

- A structured progress event for a long copy, if integrators need more than the log messages the command emits.

## Appendix: Implementation requirements

This appendix holds the implementation requirements that keep the guarantees of [Proposed Design](#proposed-design), and the current code those guarantees rely on, so that the specification and plan start from them and none is lost between the proposal and the implementation. Other sections of this proposal rely on some of them, such as the temporary directory of a copy and the write-through rename on Windows. A specification can meet a guarantee another way, as long as it says which requirement it replaces, and why the guarantee and every section that relies on the requirement still hold.

### Where external directories are used

These keep the guarantees of [Access to the external root](#access-to-the-external-root).

- Every write to an external directory happens in the service today. Creating a virtual clone refuses outside the service (`create_local` in [`repository.rs`](../../lore-revision/src/repository.rs)), deleting or moving one goes through the service's mount manager ([`instance.rs`](../../lore-revision/src/instance.rs)), and `--force` on `lore clone` and `lore repository create` refuses to replace a virtual clone's Lore directory (`force_may_replace` in [`repository.rs`](../../lore-revision/src/repository.rs)).
- Outside the service, on Linux, finding the Lore directory of a path inside a mounted FUSE clone reads that clone's external directory (`discover_external_dot_lore` in [`fuse/discover.rs`](../../lore-revision/src/fs/fuse/discover.rs)), which is how clone and create refuse a destination that's already a repository. Discovery reaches an external directory only through a live FUSE mount. Prune and listing read external directories to judge registrations, and do so under a shared store's lock.
- A service takes shared access the first time it mounts a virtual clone from a root, or creates, deletes or moves one there, and holds it until its process exits. Paths into an external root outlive both the mount table entry and the command: deferred FUSE work runs past an unmount ([lore-fuse README](../../lore-fuse/README.md)), SWFS mounts close after the service stops accepting calls ([`service_main.rs`](../../lore/src/service/service_main.rs)), and a command's repository is flushed after the command returns (`post_command_cleanup` in [`call.rs`](../../lore/src/call.rs)).
- Lookups that only read, such as prune, listing and the multiple-instance warnings, take no access in the service either: they read under a shared store's lock, and read unknown while a move is recorded.

### Stopping the service and holding its socket

These keep the guarantees of [The command](#the-command).

- As soon as the stopped service gives up the socket, the command occupies it, and only then waits for the stopped service's process to exit. The bounded wait also covers Windows releasing a file lock shortly after its holder exits.
- The command claims the socket as a service does and listens on it, so no service can start on it, whether it takes the claim ([`local_socket.rs`](../../lore-transport/src/local_socket.rs)) or binds the socket directly. If a service claims the socket between the stop and the occupation, the command refuses.
- For a service from before this proposal, which reports no process ID, the command waits only until its socket stops answering.

### The order of a move

These keep the guarantees of [The move](#the-move), [The final switch](#the-final-switch) and [The redirect at the old location](#the-redirect-at-the-old-location).

- The command decides between rename and copy by the volume identity of the current root and the target, before it writes the record. It retries the rename while the system reports files in use, as instance move and delete already retry ([Instance move and delete](2026-10-19-instance-move-and-delete.md)).
- The copy takes everything in the root, including the rename-aside directories that an interrupted instance delete leaves, and skips the contents of `write-staging/`, which FUSE empties at mount. On Linux, refusing a hard link loses nothing, since FUSE refuses to make one in a virtual clone (`link` in [`fuse/filesystem.rs`](../../lore-revision/src/fs/fuse/filesystem.rs)). The copy goes to a temporary directory beside the target, made with access for the user only on Linux, and becomes the target by one rename once its contents are on disk.
- On the first move, right after the move record, the command writes a marker from generation 1 into the current root and then stores the root ID and generation in `external-root`, each on disk before the next step.
- If the default root doesn't exist yet, as on an installation with no virtual clones, there's nothing to move, and the record names the move `create`: the command creates the target empty with the marker, then stores the root ID and generation and switches the setting to the target with one write of `external-root`, and makes the redirect.
- `--abandon` creates the new root empty with a marker from the next generation, then switches to it and raises the generation with one write of `external-root`.
- The record's kind is `rename`, `copy`, `create`, `adopt` or `abandon`, and the command updates its phase as the move passes each step.
- The final switch runs under the store locks in this order: the rename, or placing the finished copy; the switch of the setting; after a copy, removing the old root's marker and renaming what's left of the old root aside; and last the redirect. A move back to the default location removes the redirect first instead, just before the root takes its place. So when the old root is the default location, the redirect takes its place only once the old root has moved away from it.
- Before it renames what's left of the old root aside, the command adds the aside name to `external.leftovers`. After the last step of the final switch, it releases the socket and exclusive access, and removes the leftover at background disk priority, as instance delete removes an instance's files. Service start retries the leftovers after it accepts calls. Removal skips a leftover that is the root the setting names, and `--adopt` takes the root it adopts off the list.

### Taking the store locks

These keep the guarantees of [The final switch](#the-final-switch).

- Just before it takes the locks, the command checks the mount table again on Linux, and rolls the move back if a live `fuse.lore` mount of this global data directory has appeared.
- The command takes the locks one at a time in a fixed order, and waits a few seconds at most for each. When a wait runs out, it releases every lock it holds and starts over in the same order, keeping a finished copy. Store lock waits have no timeout ([ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)), so a wait without a bound could deadlock with a program that holds one of those stores while it waits for another.
- Before a copy starts, the command checks once that no store is held open.

### Durability on each platform

These keep the *Durable* property of [The move](#the-move), for the marker, `external-root`, the record, a placed copy and the redirect.

- On Linux, Lore flushes the file and the directory that holds it.
- On Windows, which has no call that flushes a directory (`sync_parent_directory` in [`instance.rs`](../../lore-revision/src/instance.rs)), Lore flushes the file and replaces it with a rename that returns only once the rename is on disk, the `MOVEFILE_WRITE_THROUGH` flag of [`MoveFileExW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw). That a rename within a volume is on disk when the call returns is an assumption (see [Risks and Assumptions](#risks-and-assumptions)).

### Recovery steps

These keep the guarantees of [Recovery](#recovery).

- Recovery picks up at the phase the record names, and never treats a path that holds the command's redirect as the old root.
- The command takes the store locks for recovery as it does for a move. Service start tries each lock once; if a store is in use, it keeps the record and waits as the Wait row does, then retries recovery in the background.
- The quick steps that run before a service accepts calls are renames, and writes of the record, the setting, markers and the redirect.
- Undo removes the marker from what the move made at the target, its temporary directory or the placed copy, and sets it aside as a leftover, then makes the redirect match the setting and removes the record.
- Finish, for a move by copy, removes the old root's marker and sets what's left of the old root aside as a leftover, unless the record shows that's done. If the old root's volume is gone, it lists the old root as a leftover, so that its marker and contents go when it returns. Then it makes the redirect match the setting and removes the record.
- A move of kind `create` counts as a rename. Before its one write of `external-root`, no root ID is stored and the missing default root counts as an available, empty old root, so recovery undoes the move. After that write, the setting names the new root, so recovery finishes it.
- An interrupted `--abandon` creates the new root if it's missing, writes its marker from the next generation, switches the setting and raises the generation with one write of `external-root` if that hasn't happened, and makes the redirect match.
- An interrupted `--adopt` switches the setting to the adopted root if it hasn't been switched, makes the redirect match, and changes nothing in either root.
- A takeover by `--adopt` lists the move's other root as a leftover, since it holds nothing the adopted root lacks: nothing mounts from either root while the move is recorded.
