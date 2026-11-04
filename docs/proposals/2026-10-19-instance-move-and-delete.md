---
lep: 2026-10-19-instance-move-and-delete
title: Instance move and delete commands
authors:
  - Matthew Hoffman
  - Zack Neyland
status: Accepted
created: 2026-10-19
updated: 2026-11-03
discussion: https://web.lore.internal.epicgames.net/epic/Lore/change-request/1113
---

# Instance move and delete commands

## Summary

Lore records every checkout of a repository, called an *instance*, in a registry kept in the repository's mutable store.
This proposal adds `lore repository instance move <target>` and `lore repository instance delete`. Each command changes
the instance on disk, or its virtual mount for an SWFS instance, and keeps the registry consistent with the result. Both
commands support the global `--dry-run` flag, which runs the same validation without changing anything. They hold the
repository's stores and directory lock only for their short registration steps, never while they rename or delete, so
they also work on Windows, where a directory with open files cannot be renamed or deleted. When another reader has the
instance's files open, they retry the rename or delete with backoff for a bounded time before failing. Delete renames a
regular instance's directory aside before removing it, so its path is free early, and removes it at background disk
priority.

## Motivation

A user who relocates a checkout today must move the directory by hand and then run `lore repository update-path` in the
new location. A user who removes a checkout by hand leaves its registration behind until
`lore repository instance prune` retires it. Both manual procedures depend on the user knowing the procedure and on the
directory being an ordinary one.

Neither procedure is available for an SWFS instance. An SWFS instance keeps its `.lore` in an external directory under
the global data directory (see [`external_dir.rs`](../../lore-revision/src/global/external_dir.rs)), and its checkout
path is a virtual mount owned by the Lore service (see [
`mount_manager.rs`](../../lore-revision/src/fs/mount_manager.rs)). There is no directory for the user to move or
delete, and moving the registered path alone leaves SWFS mounted at the old path.

A command that does the work must also leave a usable state when it fails part-way. A failed move must not leave
`instance list` naming a path that holds nothing. A delete must work on Windows, where a process cannot delete a
directory that holds files it still has open, such as the repository's lock and store files.

A large instance also takes minutes to delete. An instance of millions of files is millions of unlinks, and at normal
disk priority they compete with every interactive program on the machine for as long as the delete runs, while the
instance's path stays occupied by a directory that is no longer an instance.

## Goals / Non-Goals

### Goals

- Move a regular instance, with either local or shared stores, in one command, leaving its files and its registration
  consistent.
- Move an SWFS instance by unmounting it from the old path and mounting it at the new one.
- Delete a regular or SWFS instance and remove its registration.
- Leave the instance registered where it was when a move fails, and leave the user a clear instruction when a delete
  fails.
- Work on Windows.
- Report configuration problems under the global `--dry-run` flag without changing the instance.
- Behave the same when the service runs the command for a caller in another process, including for relative paths.
- Free a deleted regular instance's path before its files are gone, and delete a large instance without holding the
  disk from interactive programs.

### Non-Goals

- Copying an instance between volumes. A move that the operating system cannot rename fails and tells the user to move
  the directory by hand.
- Excluding other Lore commands that run in the instance at the same time.
  See [Concurrency](#non-functional-considerations).
- Checking for uncommitted changes or unpushed commits before a delete. `instance delete` does what a user does by
  deleting the directory by hand and then running `lore repository instance prune`, and neither of those steps checks
  for data loss. This proposal keeps the same behavior, so a delete is as destructive as removing the directory. The
  check is not trivial. See [Unsaved work](#unsaved-work).

## Proposed Design

### Surface

The CLI gains two subcommands under `lore repository instance`:

- `move <TARGET_PATH>` moves the instance named by `--repository` to `TARGET_PATH`.
- `delete` deletes the instance named by `--repository`.

The C API gains `lore_repository_instance_move` and `lore_repository_instance_delete`, each with an asynchronous
variant. Each reports progress as events: `RepositoryInstanceMoveBegin` and `RepositoryInstanceMoveComplete` for a move, and
`RepositoryInstanceDeleteBegin`, `RepositoryInstanceDeleteProgress` and `RepositoryInstanceDeleteComplete` for a delete. The delete progress events
carry a phase, exported in the header as `lore_repository_instance_delete_phase_t` with the values
`LORE_REPOSITORY_INSTANCE_DELETE_PHASE_DEREGISTERING`, `…_UNMOUNTING` and `…_DELETING_FILES`. The event types live in [
`instance.rs`](../../lore-revision/src/instance.rs).

Neither command defines its own dry-run option. They honor the global `dry_run` setting, which the CLI exposes as
`--dry-run` on every subcommand and the C API carries in the global arguments.

### Running without stores or the directory lock

Both commands run through `repository_call_no_store` in [`call.rs`](../../lore/src/call.rs). That access mode takes no
`FSLock` and opens only in-memory stores, so after the command reads the instance's `config`, `id` and `instance` files,
this process holds nothing open under the instance directory. This is what lets the rename and the delete succeed on
Windows. A command that needs the real stores opens them itself, for a single step, and closes them before the next
step.

### Retrying while files are in use

The commands do not keep other readers out (see [Non-Goals](#non-goals)), so a rename or delete can find the instance's
files in use: another Lore command reading the instance, in the same process or another, or a program the user has open
in it. A finished command in the service process can also keep the instance's stores open for its keep-alive window,
ten seconds by default. Rather than fail at once, each command reissues the file step that was refused, with
exponential backoff, until the reader lets go or a fixed number of attempts is spent:

- **The steps retried.** The rename of a regular instance's directory, by move or by delete's rename aside, and the
  removal of the instance directory or of an SWFS instance's external directory. A removal that is reissued continues from what the previous attempt left. The
  SWFS remount is not a file operation and is not retried. The registration steps already wait for the per-path write
  token, and are not retried either.
- **The errors retried.** Only errors that say the files are in use, so that the step can succeed once they are
  released: a busy resource on every platform, and on Windows access denied, a sharing violation or a lock violation. A
  delete also retries a directory left non-empty, which a reader creating a file during the removal leaves, as does a
  file on Windows whose deletion waits for a handle that is still open. A rename is retried only while its target is
  still free, because Windows reports a rename onto a taken path as access denied, the same error an open file gives.
  Every other error is reported at once.
- **The schedule.** Constants in [`instance.rs`](../../lore-revision/src/instance.rs) set it: the first wait is 50
  milliseconds, each wait doubles up to 2 seconds, and the step is reported as failed after 12 waits, about fifteen
  seconds in all, with the jitter the shared backoff adds. That is longer than the default store keep-alive, so a
  command that has just finished in the same process does not fail a move or delete. The schedule is not configurable.

A step that is still refused when the attempts are spent fails with the same error and instruction as before.

### Move

1. **Resolve the target.** The dispatch layer resolves a relative `TARGET_PATH` against the working directory the call
   carries, which for a call relayed to the service is the caller's, falling back to the service's own only when the
   call names none. Every later step uses the absolute result.
2. **Validate**, in dry-run and real mode alike:
    - The target does not exist and its parent does.
    - On Windows, the caller's working directory is not inside the instance. Windows refuses to rename a directory that
      is any process's working directory, so the command fails with an explanation.
    - The instance's config loads, and the shared store it names, if any, resolves.
    - For an SWFS instance, the service is running, holds the instance mounted at its current path, and does not already
      hold a mount at the target.
3. **Move.** A regular instance's directory is renamed, retrying while its files are in use (see
   [Retrying while files are in use](#retrying-while-files-are-in-use)). An SWFS instance is remounted: the service
   closes the interface at the old path, then mounts a fresh interface at the new path, and mounts the old path again if
   the new mount fails. Closing is explicit and idempotent, because other holders of the interface would otherwise keep
   the old path mounted. From the moment the close begins, no call into SWFS and no SWFS callback is admitted to the
   interface, including a callback SWFS was already dispatching. The close waits for those already in progress to
   return, however long they take, and only then releases the native handle, so nothing uses the handle after it is
   closed. Admitting a call costs one atomic increment and never blocks, because every SWFS callback and freeze or thaw
   passes through it. Only the rare close waits.
4. **Record the new path.** The command opens the instance at its new location as a repository and runs the same update
   as `lore repository update-path`. It registers the instance at the new path and retires any registration another
   instance left there.
5. **Roll back on failure.** If recording fails, the command moves the instance back and registers the old path again. A
   failure at either step is reported with an instruction to run `lore repository update-path`.

The registration is written last, so a move that fails at step 3 leaves the instance registered where it still is.

### Delete

1. **Validate**, in dry-run and real mode alike: the working-directory check on Windows, the config and shared-store
   check as in move, for SWFS that the service is running and holds the instance mounted at its path, and for a
   regular instance whether its directory is a mount point and, where it is not, that the sibling name the rename
   aside uses is free.
2. **Deregister.** If the instance uses a shared store, the command opens the instance as a repository, removes its
   registration and anchor keys, flushes, and closes it. Deleting the directory removes a registry kept in the
   instance's own local store, so the command skips this step for such instances. The command closes the instance before
   it deletes anything, so no lock or store file is open.
3. **Unmount** an SWFS instance, closing its interface.
4. **Delete the files**: the external directory for an SWFS instance, the instance directory otherwise, retrying while
   they are in use. A regular instance's directory is first renamed aside, to a sibling `<name>.deleting-<instance ID>`
   in the same parent directory, where `<instance ID>` is the hex form of the identifier in `.lore/instance`, so its
   path holds nothing as soon as the rename lands, and the command then deletes the sibling. The rename is retried
   while the files are in use, like any rename here, and a rename still refused after the retries fails the delete
   before anything is removed. Only where the instance directory is itself a mount point, which validation detects by
   comparing the device of the directory with that of its parent (the volume serial number on Windows), does it delete
   the directory in place, since no rename of a mount point can succeed.
5. **Delete at background disk priority.** The thread that removes the files lowers its own disk priority for the
   removal and restores it afterwards: `setiopolicy_np` with `IOPOL_THROTTLE` on macOS, `ioprio_set` at the lowest
   best-effort level on Linux, and `THREAD_MODE_BACKGROUND_BEGIN` on Windows, which also lowers the thread's CPU and
   memory priority. Where the system refuses, the removal runs at normal priority. An SWFS instance's external
   directory is removed the same way.

Measured on an instance of 5.0 million files, about 350 GB, on an Apple M3 Max with its internal SSD (APFS, macOS
26.6), with a release build of a prototype and the tree outside Spotlight's index: five deletes took 330 to 445 s,
while Lore commands in another instance and a build ran normally.

The command removes the registration before the files. If the delete then fails, the error names what is left (the
directory renamed aside, the instance directory where the rename was not possible, or the SWFS external directory) and
tells the user to delete it by hand. If the instance cannot be
opened to deregister it, the command stops before it changes anything.

### Unsaved work

Delete does not look for uncommitted changes or unpushed commits. Deleting the directory by hand and running
`instance prune` has the same effect and carries no such check, so this proposal does not hold the command to a higher
standard than the manual route it replaces. The check is also larger than it looks, and a later proposal could add it.
Doing it right needs at least:

- **A scan of the working tree.** The recorded dirty flags only cover files Lore has already examined, so finding edits
  made since the last scan means walking the tree. That walk persists the refreshed flags, so the check opens the
  instance with write access and holds its lock. It is not a read-only operation, and on a large checkout it makes
  delete slow.
- **A way to read the result.** The status code reports through events. The command would need a private event collector
  or an internal status function that returns counts.
- **A definition of unpushed work.** Commits are lost only when the stores are local to the instance, since a shared
  store keeps them. The unit is each branch whose local latest revision is ahead of the remote's, not only the
  checked-out one. Deciding whether a branch is ahead needs the remote, so an offline run cannot say, and the command
  would need a rule for that case.
- **An override and a policy for SWFS.** The command needs a way to proceed anyway, and a definition of local changes
  for an SWFS instance, where they live in a separate write directory.

### Dry run

Under the global `--dry-run` flag, each command runs its validation, emits its events, and stops. A move emits
`RepositoryInstanceMoveBegin` and `RepositoryInstanceMoveComplete`. A delete emits `RepositoryInstanceDeleteBegin` with its dry-run field set, the
progress events for the steps it would take, and `RepositoryInstanceDeleteComplete`. The CLI labels the output as a dry run. A dry
run of a delete checks that the sibling name for the rename aside is free, and renames nothing.

## Compatibility

- **Wire format** — The service and client exchange commands and events over local IPC, encoded with bitcode, and this
  proposal adds the move and delete commands and the instance events to that
  encoding. [ADR-00025](../developing/decisions/00025-service-version-compatibility.md) already documents how breaking
  changes to the C API like this are handled for client-service IPC.
- **Client/server protocols** — N/A for the remote protocol: no new RPCs. Recording or removing a registration opens the
  instance as a repository, which makes the same connection to the remote that other commands make.
- **On-disk format** — N/A: the commands write the same registration records and anchor keys that `update-path` and
  `instance prune` already write. Delete zeroes those keys and leaves the old registration blob in the immutable store
  until store garbage collection.
- **CLI and public API** — Adds the two subcommands and the C API functions and events described under Surface. The C
  header gains the `lore_repository_instance_delete_phase_t` type and its values. It adds no unprefixed identifiers. The rename
  aside changes what a caller of `instance delete`, which shipped in v0.12.0, observes. When the command returns, the
  files are deleted as before, but the instance's path is free earlier, from the rename on. A delete that fails part
  way leaves what remains under `<name>.deleting-<instance ID>` rather than at the instance's path, and the error names
  that directory. The rename happens within the `DeletingFiles` phase, so the phases and events are unchanged. This is
  not a breaking change: no call that succeeded before fails, and a caller that waits for the command still finds the
  files gone when it returns.
- **The `<name>.deleting-<instance ID>` name** — A new directory name beside the instance while a delete runs, visible to
  a parent instance's status, to file watchers and to backup tools. Scripts that clean up after a failed delete can
  rely on the name.

## Non-Functional Considerations

- **Concurrency** — The commands do not exclude other Lore commands in the same instance. The `NoStore` mode takes
  neither the repository lock nor a write token, so a command that opens the instance during a move or delete can race
  with it. Recording the path (move) and deregistering (delete) each take the per-path write token for their own short
  step. After deregistration releases the token and lock, a second process can open the instance before the files are
  deleted. For a regular instance the rename aside narrows that gap to the time between deregistration and the
  rename: after the rename nothing is left at the path to open. A write command that opens the instance inside the gap
  registers it again (`register_instance_and_migrate_anchors` in
  [`repository.rs`](../../lore-revision/src/repository.rs)), and prune retires that registration once the path is
  gone. On Linux and macOS, a process that already holds files of the instance open keeps them through the rename; on
  Windows open files refuse the rename, which is retried. For an SWFS instance, and where the rename is not possible,
  this proposal accepts the whole gap. A reader that only has files open while the rename or delete runs delays it
  rather than failing it, for up to about fifteen seconds (see
  [Retrying while files are in use](#retrying-while-files-are-in-use)).
- **Memory** — Preserved. The commands buffer only paths and registry records.
- **Statelessness** — The service's mount manager is the only state the commands change: they add, remove and replace
  its mounts for SWFS instances. Outside a service, the commands keep no state. The background disk priority belongs to
  the thread that removes the files, and that thread restores its earlier priority before it runs other work.
- **Latency** — A delete at background priority runs longer when other programs use the disk, and the caller waits for
  the whole removal, within the `DeletingFiles` phase.
- **Determinism** — Preserved. The commands never change an instance's identifier or any history. The recorded path is
  the cleaned absolute form that `update-path` records.
- **Durability and crash consistency** — A move and a delete are sequences of steps, not one atomic operation, and each
  registry write is flushed before the next step starts. A crash between a move's rename and its registration leaves the
  registry naming the old path, which `lore repository update-path` or `instance prune` repairs. A crash between a
  delete's deregistration and its file deletion leaves files that nothing lists, at the instance's path or under
  `<name>.deleting-<instance ID>` beside it, which the user deletes by hand. A move
  writes the registration last, so a failure before the rename leaves it correct. A delete removes it first, so no
  failure leaves a registration for files that are gone.
- **Availability** — A regular instance's files are unavailable only while the rename runs. An SWFS instance is
  unmounted between the close of the old mount and the new mount, so reads of its files fail during that window. A
  deleted regular instance's path is empty from the rename aside on.

## Migration Plan

N/A — no breaking changes, no migration required.

## Security Considerations

The commands add no new authority: they act on local files with the invoking user's privileges and change a registry the
user can already change through `update-path` and `instance prune`.

Delete removes the directory of the repository it opened, never a path read from the registry, so a crafted registry
record cannot redirect it. The sibling it renames that directory to is named from the hex form of the 16-byte
instance identifier, so the name cannot climb out of the parent directory, and validation refuses a sibling name that
is already taken. A directory created under that name after validation is one the rename replaces only where it is
empty, as a rename onto an empty directory does on Linux and macOS, so a delete never removes entries it did not
rename there.

Both commands read one value from the repository directory itself: the instance identifier in `.lore/instance`. An
instance counts as SWFS when an external directory exists for that identifier, but a directory can name any identifier.
A checkout that named an existing SWFS instance's identifier could otherwise make a delete, run by a user inside the
service, remove that instance's external directory while the instance stayed mounted. The same holds for an SWFS
checkout whose instance file is changed to name another SWFS instance: its path is still a mount, but of the instance it
belongs to, so a delete would deregister and remove the named instance's external directory while unmounting its own,
and a move would remount its own instance while registering the named one. Both commands therefore treat an instance as
SWFS only when the service holds the directory's own path as a mount of that same instance, which ties the path to the
identifier, and fail otherwise. A checkout that is not SWFS but whose path is a mount fails too, rather than having its
files moved or deleted through the mount.

## Privacy Considerations

The registry already records each instance's absolute path, visible to everyone who can read the shared store through
`lore repository instance list`. A move replaces that path with the new one. The commands add no new identifiers or
metadata. Delete zeroes the registration keys but does not delete the old registration blob, which holds the old path,
from the immutable store. That blob stays until store garbage collection, as when `instance prune` retires an entry.

## Risks and Assumptions

**Assumptions**

- **Assumption:** A rename within one volume is a single atomic operation, so a move either happens or does not. —
  *invalidated if:* a supported platform or filesystem completes a rename partially.
- **Assumption:** `NoStore` access leaves no file open under the instance directory after the command reads its
  config. — *invalidated if:* a later change to the `NoStore` path holds a handle, which would make the rename and
  delete fail on Windows.
- **Assumption:** The SWFS library releases a mount path when `swfsClose` returns, so a new mount at another path can
  start immediately. — *invalidated if:* SWFS releases the path asynchronously and the new mount races it.
- **Assumption:** Running `update-path` on an instance at its new location replaces its registered path and retires any
  registration at that path. — *invalidated if:* a registration at the new path can survive the update.

**Risks**

- **Risk:** A crash between the rename and the registration leaves the registry naming the old path. — *mitigation:*
  `lore repository update-path` in the new location, or `instance prune`, repairs it. The error messages for the
  failures the commands can detect say so.
- **Risk:** A failed delete leaves files and no registration. — *mitigation:* the error names the directory to delete by
  hand. The command accepts the leftover files for a regular instance. For SWFS the message also names the external
  directory.
- **Risk:** Another reader has the instance's files open when the rename or delete runs, and Windows refuses it. —
  *mitigation:* the step is retried with backoff for about fifteen seconds, longer than the default store keep-alive.
  A reader that holds the files longer, such as a long sync, still fails the step, with the same instruction as any
  other failure.
- **Risk:** A permission problem on Windows reports access denied, as an open file does, so a move or delete that can
  never succeed is reported only after the retries are spent. — *mitigation:* accepted. The delay is bounded, and only
  a step that would fail anyway pays it.
- **Risk:** A second process opens the instance in the gap between deregistration and deletion, and registers it
  again. — *mitigation:* the rename aside narrows the gap for a regular instance, and `instance prune` retires the
  registration once the path is gone. Accepted for an SWFS instance, which has no directory at its path to rename, and
  where the rename is not possible.
- **Risk:** A crash or a failed delete leaves `<name>.deleting-<instance ID>` beside the instance's old path. —
  *mitigation:* the instance is already deregistered, and the directory is plain files the user deletes by hand. The
  error of a failed delete names it; after a crash, the name beside the old path says what it is.
- **Risk:** A crafted `.lore/instance` file makes a command treat a checkout as another SWFS instance. —
  *mitigation:* both commands require the service to hold the directory's path as a mount of the instance the file
  names before they act on an SWFS instance, and fail otherwise. See Security Considerations.
- **Risk:** Opening the instance at its new location, or to deregister it, connects to the remote and can run
  housekeeping such as a mutable-store upgrade, which a move or delete would not otherwise do. — *mitigation:* accepted.
  `update-path` already behaves this way. The dry run opens nothing of this kind.
- **Risk:** The rollback after a failed registration is hard to trigger with a real failure, so the success path would
  exercise it far less than the rest. — *mitigation:* the step that records a path is a parameter of the move, so unit
  tests force each rollback outcome: the source registration restored, the restore failing, and the move back failing.

## Drawbacks

- An SWFS instance is unmounted between the close of the old mount and the new one, so reads in that window fail.
- Delete does not warn about uncommitted changes or unpushed commits, so it can destroy work that has not been saved.
- A rename or delete that fails because the files are in use, or on Windows because access is denied, is reported only
  after about fifteen seconds of retries.
- A delete at background disk priority takes longer on a busy machine, and the caller waits for all of it.

## Alternatives Considered

### Document the manual procedure

Keep the status quo: tell users to move the directory and run `update-path`, or to delete it and run `instance prune`.

*Rejected because:* it cannot work for SWFS instances, which have no directory to move or delete. It also gives no
validation before the change and leaves a deleted instance in `instance list` until someone prunes.

### Hold the lock and stores, and run the file step afterwards

Run the commands as ordinary write commands, then release the repository and run a deferred step. A write-call variant
would take the lock, run the command, flush and drop the repository, and then run the step the command returned.

*Rejected because:* it adds a new variant to the call framework, a way to wait for every store and lock to close, and a
pending-operation type for each command. `NoStore` access gives the same result with no framework change.

### Write the registration before moving

Update the registry first, then rename the directory or remount.

*Rejected because:* a move that fails afterwards leaves `instance list` naming a path that holds nothing, with nothing
to undo it. Writing the registration last leaves the instance registered where it is.

### Delete the files and let `instance prune` clean up

Make delete a plain directory removal and rely on a later prune to retire the registration.

*Rejected because:* the deleted instance would stay listed until someone runs prune, and prune keeps an SWFS instance's
external directory while the SWFS configuration remains.

### Rename aside before deregistering

Rename the directory first, then open it at the renamed path to remove its registration, which would close the gap
for a regular instance.

*Rejected because:* it changes the files before the registration step. A deregistration that fails, such as when the
instance cannot be opened, would have to rename the directory back, and a rename back that fails leaves the instance
registered at a path that holds nothing, the state this proposal orders its steps to avoid.

### Delete in place at normal priority

The behaviour before this amendment.

*Rejected because:* the path stays occupied for the whole removal, minutes for a large instance, and the removal
competes with interactive programs for the disk throughout.

### Return before the files are deleted

Rename aside, then remove the files in the background after the command returns.

*Rejected because:* a caller could not tell when the space comes back or whether the removal failed, and a process
that exits, such as the CLI, would stop the removal part way.

## Prior Art

Git's [`git worktree move` and `git worktree remove`](https://git-scm.com/docs/git-worktree) relocate and delete a
linked working tree and update the administrative files that record it. `git worktree repair` re-links a working tree
that the user moved by hand, which corresponds to `lore repository update-path`. Lore's commands follow the same split:
a command that does the whole operation, plus a repair step for the manual path.

## Unresolved Questions

None. A move across volumes keeps failing with the manual-move instruction, as Non-Goals states. An older service
sent a command variant it does not know fails to decode it and drops the connection, and the client reports "Lore
service closed connection without sending a result" (`lore/src/remote/call.rs`); the handshake ADR-00025 proposes
would turn that into a clear refusal.
