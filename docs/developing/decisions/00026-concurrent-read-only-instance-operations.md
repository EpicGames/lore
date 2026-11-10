---
status: proposed
date: 2026-10-29
---

# ADR-00026: Concurrent read-only commands share one instance operation

## Context and Problem Statement

Every Lore command reads and writes the working tree through an operation that its filesystem provider begins and finalizes (`with_operation` in `lore-revision/src/fs/filesystem_provider.rs`). The provider contract says a filesystem holds one operation at a time, and that the next begins only once that one is finalized (`FilesystemProvider::begin_operation`). Nothing enforces that rule.

Commands that only read the working tree — `status`, `diff`, file info, file hash — take no repository write token. They still open an operation (`with_operation_if` in `lore-revision/src/repository/status.rs`, `with_operation` in `lore-revision/src/file/info.rs`, `lore-revision/src/file/hash.rs`, `lore-revision/src/file/diff.rs` and `lore-revision/src/diff.rs`). The Lore service serves each client connection on its own task (`serve_connection` in `lore/src/remote/connection.rs`), and every SWFS instance is served from the service. Two clients can therefore have operations open against the same instance at the same time.

On disk this has no further effect: `OsOperation` holds no state, and both reads go straight to the filesystem. On SWFS it breaks the snapshot. `SwfsFilesystem::begin_operation` freezes the mount, and `SwfsOperation::finalize` thaws it, unconditionally (`lore-revision/src/fs/swfs/filesystem.rs`). When two operations overlap, the first one to finalize thaws the mount while the second is still reading what it took to be a frozen snapshot. The [SWFS FilesystemProvider LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md) deferred this choice to an ADR. How should a provider handle a request for a read-only operation while another operation is open?

## Decision Drivers

- **Snapshot consistency.** A reader must see one consistent working tree from `begin_operation` to `finalize`, which is what the freeze exists to provide. No operation may thaw a mount that another operation still reads.
- **Throughput of common read commands.** Editors and tools run `status` often, and often concurrently. Serializing them puts each one behind the full scan of the one before.
- **Freeze cost.** Each SWFS freeze/thaw cycle stops user writes to the mount for its duration. Fewer cycles are better.
- **Writers keep exclusive access.** An operation that writes the working tree (sync, reset, merge resolution) must not run alongside a reader, or the reader sees a half-applied tree.
- **No regression for on-disk instances.** The OS path should not gain new waiting unless the change buys it something.

## Considered Options

- Block: each provider admits one operation at a time, and a second request waits until the first finalizes.
- Share: concurrent read-only requests share one read-only `InstanceOperation`, and so one freeze. A writing operation waits for the readers to finish, and readers wait for a writer.
- Separate freezes: each operation takes its own freeze and snapshot.
- Status quo: keep the documented contract and rely on callers not to overlap.

## Decision Outcome

Proposed option: "Share", because it is the only option that both keeps every reader on a consistent snapshot and lets concurrent `status` and `diff` commands proceed together. It also takes one freeze for a group of concurrent readers, where "Block" takes one per reader.

The design in outline. It is not exhaustive: the outline fixes the shape of the solution and leaves the names and signatures to the implementation.

- **What makes a command read-only.** A command is read-only when it would always pass an empty set of files to `swfsThaw`, which ends a freeze without changing anything the mount presents. That is a command that never asks its operation for a change to the working tree. Writes to Lore's own metadata do not count, since the mount never sees them. Any command that might describe a file to the thaw, even on some runs only, is a writer.
- **Readers and writers as two traits.** `InstanceOperation` keeps the methods that read the working tree: `file_info`, `untracked_file_info`, `read_directory`, `holds_name_exactly`, `names_folding_to`, `names_one_entry`, `content_source`, `file_holds_content`, `infer_is_diffable`, `infer_is_conflicted` and `changes_from_filesystem_to_state`. A second trait, which extends `InstanceOperation`, holds the methods that change what the working tree presents. Those are the methods whose calls the wrapper records as a change today (`InstanceOperationImpl::record_change`): `write_node`, `set_file_to_immutable_store_contents`, `write_file`, `create_dir_all`, `make_executable`, `rename`, `remove`, `remove_recursive`, `copy_file`, `hold_state` and `committed`, along with `stored`, which only a commit calls. Every writer is therefore also a reader, and no reader is a writer. A read-only command that calls a write method fails to compile, so nothing is left to check at run time.
- **Opening an operation.** A provider hands out read operations and write operations separately, and `with_operation` / `with_operation_if` gain matching read and write forms. A read operation is typed to implement only the read trait, and a write operation implements both, so static dispatch through `InstanceOperationImpl` stays as it is.
- **Writers prove write authority.** Beginning a write operation takes the instance's `RepositoryWriteToken` (`lore-revision/src/repository.rs`), and a read operation takes none. Sync and commit already take the token (`revision/sync.rs`, `commit.rs`), so they gain no new waiting. A writing command that does not take it yet must start to. A change to the working tree becomes something only a token holder can ask for. On disk, the token's per-path mutex also keeps two writers in one process from overlapping.
- **Sharing on SWFS.** `SwfsFilesystem` puts a fair reader–writer lock around beginning an operation. The first reader freezes and creates the shared read operation. Later readers receive the same operation while it is open. The shared operation counts how many times it has been begun, and it thaws only once it has been finalized as many times as it was begun, so the last reader to finish is the one that thaws, with an empty file list by construction. That replaces today's single-use finalize (`InstanceOperationImpl::claim_finalize`, which refuses a second finalize) for read operations only. Each reader's `with_operation` still finalizes exactly once, whether its work succeeds or fails, so a failed reader neither thaws early nor leaves the count unbalanced. A write operation keeps the single-use finalize. A writer takes the lock exclusively and freezes and thaws as it does today. Because the lock is fair, a waiting writer holds back readers that arrive after it, so a steady stream of `status` calls cannot starve a sync. tokio's `RwLock` queues this way.
- **No lock on disk.** `OsFilesystem` takes no lock and keeps today's behavior. The OS offers no snapshot for a lock to protect, so waiting on the on-disk path would buy no consistency.

### Consequences

- Good, because no SWFS operation can thaw a mount that another operation is still reading.
- Good, because concurrent `status` and `diff` commands run together on SWFS, under one freeze.
- Good, because a read-only command cannot write the working tree by accident: the read trait has no write methods, so the compiler refuses the call.
- Good, because only a holder of the write token can change the working tree, on disk as well as on SWFS.
- Bad, because a long reader holds the freeze, and so holds back the user's writes to the mount, for as long as any reader in its group remains. Readers that join an open group extend it, without limit unless a writer queues. More Information describes a possible follow-up that bounds this.
- Bad, because a sync or reset waits for every reader in progress to finish before it can freeze.
- Bad, because on-disk and SWFS instances now behave differently: on disk, `status` can run during a sync and see a partly written tree; on SWFS it waits for the sync. The difference already exists today, and this ADR keeps it on purpose by leaving `OsFilesystem` unlocked.
- Neutral, because the per-operation state that `InstanceOperationImpl` collects is shared by the readers in a group. That state is the recorded modification times and the clock floor. Readers record no modification times, since they write nothing, so only the clock floor is shared, and it is read once per operation by design.

## Pros and Cons of the Options

### Block

`begin_operation` takes an exclusive per-provider lock and releases it in `finalize`.

- Good, because it is the smallest change: one lock, and no read-only flavor of operation.
- Good, because it enforces the existing one-operation-at-a-time contract exactly as written.
- Bad, because concurrent `status` calls serialize, each waiting out the full scan of the one ahead of it.
- Bad, because every reader takes its own freeze/thaw cycle, which stops user writes once per reader rather than once per group.

### Share

Described in Decision Outcome.

- Good, because readers run concurrently on one snapshot.
- Good, because a group of readers takes one freeze.
- Bad, because the trait splits in two, and each call site that opens an operation must choose between a read operation and a write operation. The compiler checks the choice.
- Bad, because a group's freeze lasts as long as its longest member.

### Separate freezes

Each operation freezes and snapshots independently.

- Good, because operations are independent of one another.
- Bad, because SWFS freezes a mount as a whole. `swfsFreeze` and `swfsThaw` act on the mount handle, and `swfsIsFrozen` reports one frozen state per handle (`lore-revision/src/fs/swfs/api_interface/generated/swfs.h`). A mount offers no per-operation snapshot to take.

### Status quo

- Good, because it costs nothing.
- Bad, because the service already lets two clients overlap, so the contract is broken in practice, and on SWFS the result is a reader working from a thawed mount.

## More Information

- The [SWFS FilesystemProvider LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md), under Non-Functional Considerations → Concurrency, defers this decision here.
- This ADR covers how operations on the working tree overlap. It does not change how commands serialize writes to Lore's own metadata, which the repository write token (`RepositoryWriteToken` in `lore-revision/src/repository.rs`) and the stores govern.
- Open before acceptance: whether a reader should join a group that a writer is already queued behind, or wait for the writer. A fair lock settles this as waiting for the writer.
- Possible follow-up: bound how long a group of readers can hold the freeze. A fair lock closes a group only once a writer queues. With no writer waiting, readers that keep arriving keep joining the open group, so the mount might never thaw and the user's writes might stay held back indefinitely. A follow-up could have the lock clear out the readers once a group has been open for some duration: it stops admitting new readers, lets the group drain and thaw, and starts the readers that arrived meanwhile on a fresh freeze. That bounds a freeze to the duration plus the longest reader in the group, at the cost of one more freeze/thaw cycle for each group the limit cuts short. The duration, and whether it should be configurable, would be for that follow-up to decide.
