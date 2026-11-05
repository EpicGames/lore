---
lep: 2026-10-29-swfs-through-the-filesystem-provider
title: SWFS Support Through the FilesystemProvider Interface
authors:
  - matthew.hoffman
status: Accepted
created: 2026-10-29
updated: 2026-11-02
---

# SWFS Support Through the FilesystemProvider Interface

## Summary

Lore materializes every file of a revision onto the local disk, so a clone of a large repository writes the whole tree before the user can open any of it. SWFS (Split Write File System) is a virtual filesystem driver: it presents a directory whose reads Lore answers on demand from its stores, and it sends the user's writes to a separate backing directory. This proposal connects SWFS to Lore through one internal seam, the `FilesystemProvider` / `InstanceOperation` traits in `lore-revision/src/fs/filesystem_provider.rs`. Every working-tree read and write that a Lore command makes goes through that seam. The OS-backed provider sits behind it first, introduced as a series of behavior-preserving changes. SWFS then arrives in five stages: clone and create, sync, commit, links and layers, and finally every remaining command and edge case. The seam sits low enough that SWFS and on-disk instances share each command's logic. It also sits high enough that SWFS can answer whole patterns, such as "what changed in this tree" or "write this node", in the way its freeze/thaw model suits, instead of emulating the OS file API call by call.

## Motivation

**Materializing the whole tree does not scale to the repositories Lore targets.** `clone` and `sync` write every file the view selects through `lore-revision/src/fs/realize.rs`. A repository of hundreds of gigabytes costs that much disk and that much write time before the first file can be used, even when the user only ever opens a small part of it. A virtual filesystem defers both costs to the first read of each file.

**Every copy of a repository pays the full cost again.** Several instances of one repository can already share one immutable store by using a shared store, so the content is stored on the machine once. Each instance still realizes its own copy of every file into its working tree, though, so a user with several checkouts of a large repository holds the same files several times over. A virtual filesystem that serves reads from the shared store keeps one copy of each file's content, however many instances present it. Only the files a user changes take extra space in an instance.

**Finding what a user changed costs a walk of the whole tree.** `status`, `stage` and `commit` find local changes by diffing the working tree against the revision (`OsOperation::changes_from_filesystem_to_state`, which runs `diff_os_filesystem` in `lore-revision/src/state/os_diff.rs`). On disk, that diff lists every directory and stats every file the view selects. It also reads and hashes the content of any file whose recorded modification time and size cannot vouch for it. The cost grows with the size of the tree, not with the size of the change, so a user who edited one file in a large repository still waits for a walk over all of it. A filesystem driver sees every write as it happens. SWFS keeps that record and hands it to Lore during a freeze (`swfsGetModifiedFileList`, `SWFSNodeModFlag_ChangedWhileFrozen`), so Lore can find a user's changes by reading only what changed.

**SWFS has a model that the OS file API does not express.** The SWFS C API (`lore-revision/src/fs/swfs/api_interface/generated/swfs.h`) has four parts:

- Reads arrive as callbacks, `read_file` and `fill_dir_begin`, which Lore answers.
- Writes land in a write directory that SWFS owns.
- Lore changes what the mount presents only between `swfsFreeze` and `swfsThaw`. The thaw takes a list of `SWFSFile` entries, each carrying modification flags (`SWFSNodeModFlag_*`).
- During a freeze, Lore can ask which files the user modified (`swfsGetModifiedFileList`) and read their frozen content (`swfsReadFrozenFile`).

An integration that treats the mount as an ordinary directory and writes files into it would send every file through the driver into the write directory. That defeats the virtualization. An integration that walks the mount to find changes would pay for the walk described above, and would also hydrate every file whose content it reads, which throws away the modified-file record SWFS already keeps.

**Separate SWFS code paths in each command would let behavior drift.** Lore's commands (`sync`, `commit`, `stage`, `status`, `reset`, `merge`) encode many rules: view filtering, case folding, executable bits, dirty and staged flags, recorded modification times, dry runs, and local-modification checks. A second copy of `sync` or `commit` for SWFS would have to reproduce every one of those rules and keep them in step. Bugs would then appear in one filesystem and not the other.

**The seam itself is a large change to code that works today.** Some 45 call sites in `lore-revision` reach the filesystem provider (`grep -rn "file_system()" lore-revision/src`), and most of them read or write the working tree. Routing all of them through a new interface at once, and adding SWFS in the same change, would make any regression hard to attribute and hard to revert.

## Goals / Non-Goals

### Goals

1. **G1 — One logic path.** A Lore command runs the same code whether the instance is on disk or SWFS-mounted, and only the provider's answers differ, so that behavior is identical by construction rather than by parallel maintenance.
2. **G2 — Room to optimize.** The interface lets a provider answer coarse-grained questions, such as a whole-tree diff, a node write, or a content comparison, in its own way. A provider is never forced to emulate the operations an OS file API offers one at a time.
3. **G3 — A consistent snapshot per operation.** A command sees the working tree through one operation, which a virtual provider can back with a freeze, and the operation is always finalized (thawed), including on failure.
4. **G4 — No regressions while the seam is introduced.** Every change that moves on-disk behavior behind the interface is behavior-preserving and lands on its own.
5. **G5 — Staged SWFS delivery.** SWFS support ships in stages that each stand on their own. Clone and create come first, then sync, then commit, then links and layers, and last full command coverage. Each stage is usable without the next.
6. **G6 — No cost to on-disk instances.** The OS path pays no per-file dynamic dispatch or allocation for the abstraction. Its cost is one provider call and one `Arc` per operation.

### Non-Goals

- Other virtual filesystems (ProjFS, FUSE, macOS File Provider). The interface does not preclude them, but this proposal designs only for SWFS.
- Shipping or distributing the SWFS driver itself. The driver is not yet publicly available (`docs/release-notes.md`, "Partial support for SWFS").
- Running SWFS instances outside the Lore service. Mounts live in the service process (`lore/src/service/initialization.rs`), and this proposal does not change that.
- Selective freezing. The freeze callback currently lets every file freeze (`SwfsInterface::freeze_callback` in `lore-revision/src/fs/swfs/api_interface.rs`). Freezing only the files an operation touches is a later optimization.
- Changing how layers, links, or dependency-set syncs behave on disk.

## Proposed Design

### The seam: provider and operation

The interface has two traits (`lore-revision/src/fs/filesystem_provider.rs`):

- **`FilesystemProvider`** creates operations. `begin_operation` returns an `Arc<InstanceOperationImpl>`. For `OsFilesystem` this is a stateless wrapper around the working-tree root. For `SwfsFilesystem` it is where the provider freezes the mount. One provider covers a whole mounted filesystem, so links and layers below it share the operation their parent holds.
- **`InstanceOperation`** performs reads and writes against the operation's view of the working tree.

`with_operation(filesystem, work)` and `with_operation_if(filesystem, needed, work)` are the only ways commands open an operation. They call `finalize` whether or not `work` succeeds, so a failure never leaves a mount frozen. They report the work's own error ahead of a finalize error (G3). `with_operation_if` lets a command that reads the tree only under some conditions skip the freeze entirely.

`InstanceOperationImpl` wraps a `StaticDispatchInstanceOperation` enum (`Os`, `Swfs`, and `Test` under the `test-util` feature). It forwards each trait method through a `match`. `InstanceOperation`'s async methods return `impl Future` and are not boxed, so the OS path compiles to direct calls (G6). Concerns that every provider shares live in the wrapper and not in each provider:

- **Dry run.** Every write method returns early when the execution context is a dry run. No provider has to implement dry-run handling itself.
- **Change tracking.** Each write records that it happened (`record_change`), and `finalize` passes `changes_made` to the provider. SWFS uses this to decide whether the thaw invalidates anything.
- **Recorded modification times and the clock floor.** These are collected per operation (`record_modified_time`, `clock_floor`), so the caller that knows which revision the operation leaves current commits them.

`RepositoryContext::file_system()` (`lore-revision/src/repository.rs`) returns the instance's provider. It defaults to `OsFilesystem` for the working-tree path. It is `InvalidFilesystemProvider` (`lore-revision/src/fs/invalid_filesystem_provider.rs`) for contexts that must never touch a working tree, such as the store-holder contexts and the context a mount uses internally. If code reaches the filesystem by mistake from one of these contexts, it gets an error instead of silently reading the wrong directory.

### Interface altitude

The second goal is the reason the interface is not a 1:1 copy of an OS file API. Each method sits at the level where the commands share logic and where SWFS has something better to do than the OS:

| Method | Why it sits at this level |
|---|---|
| `changes_from_filesystem_to_state(FilesystemDiffContext) -> ChangeStream` | Takes a whole subtree diff, with the filter verdicts, mounts and dirty/stage intent. `OsOperation` walks the disk. A provider that knows what the user modified answers from that list instead of walking, and so never hydrates the files it skips. |
| `write_node(repository, node, path) -> FileInfo` | One call creates the parent directory, writes the content and applies the mode. Making these four separate calls would resolve the path four times. A virtual provider records the node and never writes the bytes. |
| `set_file_to_immutable_store_contents(repository, address, path)` | Names content by address, not by bytes. This lets SWFS make sure the content is in the store and leave the read callback to serve it. |
| `file_holds_content(repository, path, previous, size, established) -> NodeComparison` | Asks "does this file hold that content?" rather than "give me the bytes". OS reads and compares. A provider that keeps a record of what it served answers without reading. |
| `content_source(path) -> ContentSource` | Returns *where* content is read from, not the content. The caller never names a host path that the provider cannot read through. |
| `holds_name_exactly`, `names_folding_to` | Case resolution as two questions: one lookup for the common case and a directory read only when needed. A provider with an index answers either without listing. |
| `read_directory(path) -> DirectoryListing` | A lazy listing, so a consumer that stops early stops the walk. |
| `file_info`, `untracked_file_info` | `untracked_file_info` is for paths the revision does not track, such as merge sidecars and scratch files. A virtual provider holds no node for them and reads them from disk instead. |
| `rename`, `remove`, `remove_recursive`, `create_dir_all`, `make_executable`, `write_file`, `copy_file` | Fine-grained primitives remain for the operations that really are per-path: merge resolution, reset, obliterate. |
| `InstanceOperationImpl::committed_path(path)` | Commit calls it for each file it stores and each node it deletes. The SWFS provider records the path as written, so the thaw that ends the commit names it and SWFS drops its record of the change. The OS provider ignores it. A dry run makes no call. |

The rules that make two providers behave the same stay in the commands: filter folding, mode derivation (`FileInfo::mode`, `mode_differs_from`), conflict and diffability inference (default methods derived from `content_source`), and local-modification checks. The provider answers questions about the tree but never decides policy (G1). Where a provider can answer a derived question more cheaply, it overrides the default method (`names_folding_to`, `infer_is_diffable`, `infer_is_conflicted`). It does not get a separate code path.

### Phase 0 — the OS provider, as a series of no-op changes

This phase addresses G4 and G6. It ships before any SWFS behavior and changes nothing a user can observe.

1. **Introduce the traits and `OsFilesystem`/`OsOperation`** (`lore-revision/src/fs/os/mod.rs`). Each `OsOperation` method calls the same `lore-io` driver call the command made before. `RepositoryContext` gains a `file_system` field defaulting to `OsFilesystem::new(path)`, and server contexts get an empty-path provider. Nothing calls the new code yet.
2. **Move call sites one command at a time.** Each change wraps a command's working-tree access in `with_operation(repository.file_system(), …)` and replaces direct I/O with operation methods: `file/info.rs`, `file/hash.rs`, `file/dirty.rs`, `file/stage.rs`, `file/unstage.rs`, `file/reset.rs`, `file/write.rs`, `file/obliterate.rs`, `file/diff.rs`, `diff.rs`, `commit.rs`, `branch/merge.rs`, `revision/sync.rs` and `fs/realize.rs`. With only `OsOperation` behind the seam, each change is a refactor. The existing unit and smoke tests are the oracle, and a regression can be traced to one reviewable, revertible change.
3. **Lift the shared concerns into `InstanceOperationImpl`** (dry run, change tracking, modified times) as separate changes, each removing the duplicate handling from the command it replaces.
4. **Add a test provider** (`StaticDispatchInstanceOperation::Test`, `filesystem_provider::test_util`). Tests use it to assert operation boundaries: each command finalizes exactly once, reports `changes_made` correctly, and finalizes after a failure. See `lore-revision/tests/unit/fs/filesystem_provider.rs`.

Once phase 0 is done, every working-tree access is routed through one point that a second provider can occupy.

### SWFS plumbing shared by every stage

- **Configuration.** `RepositoryConfig.vfs: Option<VfsConfig>` with `VfsType::{None, Swfs}` (`lore-revision/src/repository.rs`). `RepositoryConfig::validate` refuses SWFS without a shared store, because the mount serves reads from the immutable store. This is also what lets several SWFS instances of one repository share one copy of each file's content: every mount reads from the same store, and only an instance's write directory holds anything of its own.
- **Where an instance lives.** An SWFS instance's `.lore` and its SWFS `write` directory live in the external instance directory (`external_dir_for_instance`), not inside the mount.
- **Mount lifecycle.** `MountManagerState` (`lore-revision/src/fs/mount_manager_state.rs`) holds one `MountManager` per service process. The service initializes it on startup (`lore/src/service/initialization.rs`) and remounts every SWFS instance it finds in the shared-store registry (`MountManager::mounts_of_entries`). Opening an SWFS instance outside the service is an error (`repository.rs`, "Loading a SWFS repository without using the service").
- **Serving reads.** `MountResources` (`lore-revision/src/fs/swfs/mount_resources.rs`) holds the mount's own `RepositoryContext` and the `State` it presents. `read_file` resolves the path to a node and reads the requested range from the immutable store. `enumerate_directory` lists a node's children with their sizes and recorded modification times.
- **What the user changed.** `ModifiedTree` (`modified_tree.rs`) holds the files SWFS reports as changed as an in-memory `State` that carries only `Dirty*` flags, apart from the staged `State`. The `notify_*` callbacks feed it, and it rebuilds from `swfsGetModifiedFileList` where it may have missed a change. A move is a deletion of the origin and an addition of the destination. SWFS lists the origin as a deletion carrying the destination's `file_index`, and Lore pairs the two ([ADR-00029](../developing/decisions/00029-swfs-modified-file-record.md)).
- **A rule for every stage.** Lore does not walk an SWFS instance's tree through the OS filesystem. A walk lists every directory and reads what it finds, which fills SWFS's cache with the tree the mount exists to leave unfetched. Reads come from the served tree, the immutable store, the frozen snapshot (`FrozenSnapshot`, `swfsReadFrozenFile`) and the operation's own writes. The mount is read through the OS only where nothing else holds the answer, such as a record that failed to rebuild, and a path the operation wrote itself.
- **Never write the revision's own changes through the OS.** In the SWFS code paths, Lore does not write, rename or remove a file or directory of the repository through the mount path to apply a change to what a revision serves. A write through the mount is indistinguishable from the user's own change: SWFS records it as modified and reports it as one, so Lore's changes and the user's become one set. Freeze and thaw are atomic only if the changes that arrive through the mount are the user's while Lore describes its own to the thaw. Mixing them breaks what Lore needs from that atomicity: that a thaw commits exactly what Lore names, that what SWFS reports modified is the user's work, and that a user's uncommitted change is never replaced by a description of the source. What a revision holds, and what a sync, clone, branch switch or commit changes in it, such as a write, a move, a case change or a removal, is described to the thaw. Two kinds of write go through the mount. Scratch files, such as the `~base`, `~mine` and `~theirs` copies a merge leaves for the user, are not part of any revision. And an edit Lore makes on the user's behalf, whose result differs from the current revision, is a user edit: a merge or resolve result, a cherry-pick or `revert --no-commit` result, and the content a dependency sync keeps from an older revision. SWFS holds these as the user's changes, which they are, and they are read, staged and committed as such. What the user holds is removed by naming it in a thaw.
- **Limits.** A service holds at most eight SWFS mounts, because the driver limits them machine-wide, and refuses a ninth instance (`MountManager::MOUNT_LIMIT`). An operation started inside another on the same mount is refused, because SWFS refuses a second freeze ([ADR-00034](../developing/decisions/00034-swfs-serve-links-and-layers.md)). Mount system calls run off the shared task pool ([ADR-00033](../developing/decisions/00033-swfs-mount-syscalls-off-the-shared-pool.md)). The mount asks SWFS for two write threads.

### Stage 1 — clone and create

Goal: serve a revision through SWFS. Freeze/thaw has to work only in its simplest form: the mount starts empty, so nothing the user wrote needs reconciling.

- `create_local` and the clone path call `MountManager::create_mount`, which mounts an empty view of the instance, and then `get_mount_filesystem_provider`, which builds the `SwfsFilesystem` that the new `RepositoryContext` holds.
- Clone runs its ordinary realize path through the operation. `SwfsOperation::write_node` and `set_file_to_immutable_store_contents` make sure the content is in the local store (`immutable::read`) and record the path as touched. They write no bytes to the mount. `create_dir_all`, `write_file`, `make_executable` and `remove_recursive` only record the path.
- `finalize` thaws (`SwfsInterface::thaw`). It reloads the presented `State` from the current anchor (`MountResources::update_state`). It describes every touched path and its ancestors, sorted parent-first, to `swfsThaw`, which SWFS requires (`swfs/state.rs::written_files`). At this stage the thaw only publishes the tree, because the mount held nothing a user could have changed.
- `SwfsOperation::file_info` answers from the presented `State`. `update_state` reloads that `State` only inside `thaw`, so within an operation it still holds the tree from before the operation began. In stage 1 the mount starts empty, so every path the clone writes reads back as absent until the thaw. Clone tolerates this. `set_file_to_immutable_store_contents` reports no file information, so `set_file_to_node` (`repository/clone.rs`) falls back to `file_info` and finds no file. It therefore records a modification time of zero and leaves the executable bit alone, since an absent file reports no bit to compare against. The sync realize path reads nothing back: it takes the `FileInfo` that `write_node` builds from the node (`fs/realize.rs`). Stage 5's modification-time edge case covers making read-backs see what the operation has written.
- The working-tree diff and content reads (`changes_from_filesystem_to_state`, `read_directory`, `content_source`, `file_holds_content`) stay delegated to `OsOperation` (`fake_with_os!` in `swfs/filesystem.rs`). Stage 1 does not claim that `status`, `stage` or `commit` are correct on an SWFS instance.
- Clone refuses `--dry-run` with `--vfs swfs`, because a mount cannot be taken back (`repository/clone.rs`).

What users get: `lore repository clone --vfs swfs` and `lore repository create --vfs swfs` produce a browsable working tree whose files load on first read.

### Stage 2 — sync

Goal: move a mounted instance from one revision to another by updating what SWFS presents, without materializing files and without overwriting the user's local changes.

- `sync` runs the ordinary realize path (`fs/realize.rs`) through the operation, with the same local-change checks, options and per-file events as on disk. Nothing in `sync` is specific to SWFS.
- Each write the realize path makes records an intent instead of writing bytes ([ADR-00031](../developing/decisions/00031-swfs-realize-operation-results-the-revision-does-not-hold.md)). At finalize, the provider realizes the results the revision does not hold, such as merge sidecars, to the mount ([ADR-00030](../developing/decisions/00030-swfs-merge-writes-go-to-the-filesystem.md)). It then loads the served tree for the new revision and describes each path the operation touched to `swfsThaw`, as that tree serves it: a file with its size and times, a directory as a directory, and a path the tree no longer serves as holding nothing.
- A path the operation left alone, where the old and new revisions differ and SWFS holds no change, keeps what the working tree held: a path the old revision served has that content written to the mount, and a path only the new revision has is recorded as absent and not served (`keep_unwritten_changes`, [ADR-00032](../developing/decisions/00032-swfs-serve-views-and-partial-trees.md)). This is what a disk sync does when it leaves a file alone.
- To know which paths carry local changes, the provider reads the modified-file record (`ModifiedTree`), which SWFS's own list backs. It needs the paths, and the list does not carry content.
- A path that changes between file and directory in one sync, or only in the case of its name, is described as its end state in a single thaw. SWFS replaces the node and takes the spelling the description gives it (`swfs.h`, `swfsThaw`). Lore adds no step of its own for either.
- Dependency-set syncs, resets of local changes and syncs that carry layers need nothing more than the ordinary path, so `sync` refuses none of them on SWFS.

What users get: `lore sync` on an SWFS instance, with the same local-change refusals and options as on disk.

### Stage 3 — commit

Goal: read the user's writes out of a frozen mount and commit them through the ordinary commit path.

- `changes_from_filesystem_to_state` answers from the modified-file record instead of walking (`record_diff.rs`). This is the optimization G2 exists for: `status`, `stage` and `commit` visit only what the user touched, and never hydrate an unmodified file. A record that may have missed a change is untrusted until rebuilt from SWFS's list.
- `content_source` and `file_holds_content` answer through a view of the operation (`SwfsView`, `view.rs`). A file SWFS holds changed is read from its frozen snapshot (`swfsReadFrozenFile`), a file the operation wrote comes from the operation's own writes, and anything else comes from the served tree and the immutable store, so an unmodified file is never read through the mount.
- Commit names each file it stores and each node it deletes (`InstanceOperationImpl::committed_path`). The thaw that ends the commit describes them as the committed revision serves them, so SWFS drops its record that the user changed them. Without this, SWFS keeps the change, and a later thaw naming a path SWFS still holds as deleted is taken as the commit of that deletion, not as new content.
- A write the user makes during a freeze is not Lore's to describe. The thaw leaves it alone, so it behaves as if the user wrote it after the thaw, and it appears in the next modified-file list. Where what Lore expects conflicts with such a write, for example a different spelling, the user's version wins, and `reset` returns to Lore's.
- `commit` of a link alone or of a layer needs no refusal, because the provider needs no revision handed to it: the next thaw loads the served tree from what the instance has stored.

What users get: `status`, `stage`, `commit` and `reset` on an SWFS instance, with the same semantics as on disk.

### Stage 4 — links and layers

Goal: a mounted instance presents, syncs and commits its links and layers as an on-disk instance does.

A link places another repository's tree at a path in the parent (`LinkMountInfo`), and a layer overlays another repository's files at a path (`LayerMountInfo`, `lore-revision/src/state.rs`). Each has its own `RepositoryContext` and `State`. On disk they share the parent's provider and operation: one provider covers the whole mounted filesystem (`FilesystemProvider::begin_operation` docs). Stage 4 keeps that shape. A link or layer stays a subtree of the parent's mount and is never a second mount ([ADR-00034](../developing/decisions/00034-swfs-serve-links-and-layers.md)).

- **Serving.** The served tree resolves a path across the link and layer mount points. A path below a link reads its node from the linked repository's state, at the pin the staged state holds, and its content from that repository's store. A path below a layer reads from the layer's current revision where the layer holds a node there. Their files stay virtual.
- **Sync and mount changes.** The ordinary realize path covers links and layers. When a link or layer is added, repinned or removed, the thaw describes the mount path as the directory it mounts, and names the files below it only where the user changed them, rather than naming every file of a repinned tree. A local change below a link is kept, as on disk. Removing a link or layer, and `reset --purge`, name each file SWFS holds of the user's below the path in the same thaw, which deletes it, because naming a directory leaves what the user holds below it. A directory the user made stays, which no thaw removes.
- **Commit.** A commit of a link alone or of a layer is allowed, with no refusal. The thaw after it loads the served tree from what the instance stored, so the new pin or revision is served.
- **Change detection.** The modified-file record holds paths below a link or layer as it holds any path, so a diff that crosses a mount point is answered from the record. A diff the record does not answer, such as one measured from another revision, walks the operation's own state rather than the mount.

What users get: repositories that use links and layers work on SWFS, with the same sync and commit behavior as on disk.

### Stage 5 — every command, and the edge cases

Goal: every `lore` command either works on an SWFS instance through what stages 1–4 built, or refuses with an error that names what it cannot do. The behavior differences that remain between the two providers are found and closed.

- **Command coverage.** Stage 5 checks each command in `docs/reference/lore-cli-commands.md` that reaches the working tree against an SWFS instance. The per-path commands, such as file reset, `obliterate`, file write and merge resolution with its `~base`/`~mine`/`~theirs` sidecars (`BASE_SUFFIX`, `MINE_SUFFIX`, `THEIRS_SUFFIX` in `lore-revision/src/repository.rs`), run on the fine-grained primitives in the interface. Each records the path it wrote or removed, and the thaw describes it.
- **No refusals of its own.** Nothing on the SWFS path refuses a dependency-set sync, a sync that resets local changes, a sync that carries layers, or a commit of a link or layer alone.
- **No OS fallbacks.** No `SwfsOperation` method delegates to `OsOperation` (the `fake_with_os!` macro in `swfs/filesystem.rs` is gone). `untracked_file_info` answers from the frozen snapshot: SWFS freezes every file it holds changed, ignored files included, so an untracked path the snapshot lacks is not there. Only a path the operation wrote itself, or a snapshot that could not be taken, asks the mount.
- **Edge cases**, each settled to match the OS provider or documented where it cannot:
  - case-only renames, and two spellings of one name, on a mount that folds case;
    - `stage --case keep` does not rename the file at stage time. The staged content takes the repository's spelling, and the commit's thaw names the path with the committed state, which SWFS applies with the spelling the description gives it. A tracked path that SWFS holds a change at is therefore not renamed through the mount (`SwfsOperation::rename`).
  - executable bits, which the mount presents from the node's mode;
  - recorded modification times, where `SwfsOperation::write_node` reports none of its own;
  - a sync that changes the view, which changes what the mount presents without changing the revision;
  - a service restart, an instance move or an instance delete during a freeze;
  - content evicted from the shared store while a mount still presents it.
- **One test suite for both providers.** The command tests run against an on-disk instance and an SWFS instance alike, so the behavior G1 promises is checked rather than assumed.

What users get: an SWFS instance that behaves like an on-disk one for every command, with any remaining differences documented.

### Goal traceability

| Goal | Addressed by |
|---|---|
| G1 | The seam; policy stays in commands; the ordinary realize and commit paths shared by both providers in stages 2–4; stage 5's shared test suite |
| G2 | Interface altitude table; stage 3's list-driven diff; stage 1's no-byte writes |
| G3 | `with_operation` / `with_operation_if`; freeze in `begin_operation`, thaw in `finalize` |
| G4 | Phase 0 |
| G5 | Stages 1–5 |
| G6 | `StaticDispatchInstanceOperation`; unboxed async methods |

## Compatibility

- **Wire format** — N/A. The provider affects only how an instance reads and writes its working tree. It does not change what goes over the wire.
- **Client/server protocols** — The service's status reply gains `swfs_mount_count` (`lore/src/service.rs`). The CLI-to-service IPC carries `LoreVfsType` (`None`, `Default`, `Swfs`) on create and clone arguments (`lore/src/repository.rs`). Lore servers see no change.
- **On-disk format** — `RepositoryConfig` gains an optional `vfs` table. An absent table means `VfsType::None`, so existing repositories read as before. SWFS instances keep `.lore` and `write/` in the external instance directory (`external_dir_for_instance`), not at the mount path. Only the service's mount manager maps a mount path to that directory (`get_dot_lore_path` in `lore-revision/src/repository.rs`). Any other client looks for `.lore` at the working-tree path, and the mount presents only the revision's tree (`MountResources::enumerate_directory`). That covers a stale client that predates SWFS support, and a client running without the service. Such a client does not see a mounted SWFS instance as an instance at all, and fails there as it would in any directory that holds no repository. It never reads an SWFS instance's state as though it were an on-disk one, so it cannot misreport or rewrite that state. The one thing it can still do is create or clone a new repository at the mount path, which would write a `.lore` through the mount into the SWFS write directory. Other instances are unaffected. No fragment, index or schema format changes.
- **Build configuration** — Everything that links to SWFS is behind the `swfs` Cargo feature. Without the feature, the types still exist and their methods return errors (`lore-revision/src/fs/swfs/api_interface.rs`), so a build without SWFS compiles the same code paths and fails to mount an SWFS instance at runtime (`SwfsInterface::mount`).
- **CLI and public API** — `lore repository create` and `lore repository clone` gain `--vfs <none|default|swfs>`, defaulting to `none` (`docs/reference/lore-cli-commands.md`). `default` currently resolves to SWFS (`LoreVfsType::to_config`). `--dry-run` with `--vfs swfs` is refused on clone. Creating or cloning an SWFS instance inside another repository's working tree, or a repository inside an instance's mount, is refused, as is a ninth instance in one service. An SWFS instance refuses to open outside the service. Nothing changes for instances created without `--vfs`.

## Non-Functional Considerations

- **Concurrency** — A filesystem holds one operation at a time, and the next begins only after that one is finalized (`FilesystemProvider::begin_operation` docs). For SWFS this means one freeze at a time per mount. Commands that only read the working tree, such as `status`, `diff` and file info, can ask for an operation while another is in progress. A future ADR will decide how to handle such concurrent reads ([ADR-00026](../developing/decisions/00026-concurrent-read-only-instance-operations.md), proposed). One option is for the second request to block until the operation in progress finalizes. The other is for concurrent readers to share one read-only `InstanceOperation`, and so one freeze. `finalize` can be called only once (`claim_finalize`), so a second caller cannot thaw a freeze that someone else still holds. SWFS read callbacks run concurrently with an operation and read the presented `State`, which changes only inside `thaw` (`update_state` under an `RwLock`).
- **Memory** — The OS path is unchanged. SWFS holds one deserialized `State` for each repository a mount presents, each of which pages nodes on demand. That is one `State` for the parent through stage 3, and from stage 4 one more for each link and layer, so the count grows with the number of links and layers, not with the size of the tree. It also holds the modified-file record, which grows with the files the user changed, and a set of absent paths. Each operation holds a set of the paths it wrote that grows with those paths, not with repository size, and a frozen snapshot of the files SWFS holds changed. A frozen file's content is held whole in memory when it is read, because `swfsReadFrozenFile` fills one buffer for the whole file. Stage 3's list-driven diff scales with the number of modified files, where a disk walk scales with the tree.
- **Statelessness** — `OsFilesystem` holds only its root. SWFS introduces process-level state that persists across operations: the `MountManager` global, one `MountResources` per mount with its presented `State`, and the native handle. That state is confined to the service process and rebuilt from the shared-store registry on startup.
- **Determinism** — Commands produce the same revisions and history whichever provider is behind them, because the policy that decides them stays in the commands. SWFS changes only where file bytes live, not what is committed.

## Migration Plan

N/A — no breaking changes, no migration required. SWFS is opt-in per instance, and existing instances keep `OsFilesystem`.

## Security Considerations

The trust model does not change for on-disk instances. For SWFS:

- The mount serves content from the local immutable store, which verifies what it reads as it does for any other read. A user writing into the mount writes into SWFS's write directory and never into Lore's stores.
- SWFS calls into Lore through C callbacks (`handle_redirect!` in `api_interface.rs`). Each callback looks up its interface by handle, enters the `HandleGate`, and dereferences raw pointers that SWFS provides. The safety of those dereferences depends on SWFS honoring the contracts in `swfs.h`. A misbehaving driver is inside the trust boundary, as any kernel driver is. A close never frees the handle under a callback: `HandleGate` (`lore-revision/src/fs/swfs/handle_gate.rs`) keeps the native handle open for every call and callback in progress, and refuses new ones once a close begins.
- Mount paths and SWFS paths are parsed through `MountPath` and `SwfsPath` (`swfs/paths.rs`) before being resolved against the `State`. A path that names no node resolves to nothing, not to a host path.
- Lore refuses to delete or move an SWFS instance in a way that would remove a mounted instance's `.lore` out from under it (`lore-revision/tests/instance_delete_swfs_guard.rs`).

## Privacy Considerations

None of this design exposes new data to other parties. Every structure the provider adds stays on the local machine: the presented `State`, the touched-path sets, and the modified-file lists. None of it is sent to servers or telemetry beyond the existing `swfs_mount_count` in the local service status. SWFS's write directory holds the same user content that an on-disk working tree would, in the external instance directory. Deleting the instance removes it.

## Risks and Assumptions

**Assumptions**

- **Assumption:** SWFS can report, during a freeze, exactly which files the user created, modified, renamed or deleted since the last thaw (`swfsGetModifiedFileList`, `ChangedWhileFrozen`). — *invalidated if:* the list misses changes, for example writes in flight at freeze time. Stages 2 and 3 would then lose local changes or fail to commit them.
- **Assumption:** Content that a thaw describes is in the immutable store before a read callback asks for it, which `write_node` ensures by caching it. — *invalidated if:* store eviction removes cached content from an SWFS instance's shared store. Reads would then need the remote, and they fail while offline.
- **Assumption:** `swfsThaw` honors the contracts `swfs.h` states, so the design adds no workaround for them. A described entry takes the type and the spelling the description gives it, so a path can change between file and directory, or change case, in one thaw. A move lists its origin as a deletion with the destination's `file_index`. A thaw that names a path SWFS holds as changed commits that change. — *invalidated if:* SWFS keeps the node it holds, or reads a description of a restored path as the commit of its deletion. A sync that turns a file into a directory, or restores a deleted file, would then leave the old state on the mount.
- **Assumption:** A thaw can discard what the user holds. Naming a user-held file as nothing, or with the `Deleted` flag, deletes it. Naming it with the source's size makes `read_file` serve the source's bytes. Naming or relisting a parent does not touch the user's files below it, so a purge names each user-held file, taken from the modified list. Naming a path discards the user's version, so Lore names user-held paths only to commit them or to discard them (reset, forced sync, purge). — *invalidated if:* a thaw leaves a user-held file in place after it is named. Verified against the SWFS build in use. **This holds for files only.** No thaw form removes a directory the user made: naming it as nothing, with the `Deleted` flag, naming its parent (relisted without it), or naming parent and child together all leave it, and leave any file in it that was not named. The design does not meet this yet, and it needs a thaw entry from SWFS. Until then `reset --purge` of an untracked directory and the purge forms of `link remove` and `layer remove` leave an empty directory behind.
- **Assumption:** A freeze over the whole mount is cheap enough to take once per command. — *invalidated if:* freeze latency grows with tree size or open handles, which would make selective freezing a requirement and no longer an optimization.

**Risks**

- **Risk:** An `OsOperation` method diverges subtly from the direct I/O it replaced during phase 0, for example in error mapping or case handling. — *mitigation:* Most operations' implementation can be 1:1 copied from the code they replace due to existing code re-use. In edge cases the author's judgement can match up error cases, with the existing test suite as the oracle.
- **Risk:** A command reaches the working tree without going through the operation and bypasses SWFS. — *mitigation:* the contexts that must not touch a working tree carry `InvalidFilesystemProvider`.
- **Risk:** A thaw fails partway through, leaving SWFS presenting a state that differs from Lore's current anchor. — *mitigation:* `finalize` always clears the touched set and reports the failure. A mount found frozen when it is mounted is thawed, and the first operation after a remount rebuilds the modified-file record from SWFS's own list.
- **Risk:** Stage 1 instances run `status`/`stage`/`commit` through the OS-delegated reads before stage 3 lands, and get incorrect results. — *mitigation:* Explicitly inform early adopters about SWFS support and ensure it is only used as supported. The limited release of the EpicVFS driver limits early users to people in close contact with the Lore team.

## Drawbacks

- Every working-tree method now passes through a `match` over providers, and adding a provider means touching every forwarding arm in `InstanceOperationImpl`.
- SWFS instances depend on the Lore service and a shared store, which on-disk instances do not.
- On SWFS, an empty directory the user made survives a purge, a forced sync and a link or layer removal, until SWFS offers a way to remove it. It shows in `status` as an add. Files the user made are removed.

## Alternatives Considered

### Treat the SWFS mount as an ordinary directory

Mount SWFS at the working-tree path and keep `OsFilesystem`. Lore would write and walk files through the driver like any other directory.

*Rejected because:* every write during clone and sync would pass through the driver into the write directory, which costs the same disk and time as materializing and removes the reason for SWFS. Every status walk would hydrate the files it reads. A plain directory has no freeze/thaw window, so Lore could not update the presented tree atomically against user writes.

### A low-level VFS trait mirroring the OS file API

Define the seam as `open`/`read`/`write`/`stat`/`readdir` and have SWFS implement each one.

*Rejected because:* that altitude cannot express "diff this subtree", "write this node" or "does this file hold this content". SWFS would have to emulate a disk file by file, and could never answer from its modified-file list or skip writing bytes. Nor can it express an operation as a freeze window. G2 would be unreachable.

### SWFS-specific command implementations

Branch on `config.is_swfs()` inside `sync`, `commit`, `status` and the rest, and write SWFS versions of each.

*Rejected because:* the rules those commands encode (filters, case folding, modes, dirty/stage flags, local-modification checks, dry runs) would exist twice and drift apart. That is the failure G1 exists to prevent. Under the chosen design, a provider that holds state still shares `hold.rs`, the commit path and the event reporting.

### Dynamic dispatch (`Arc<dyn InstanceOperation>`)

Make `InstanceOperation` object-safe and box each async method's future.

*Rejected because:* the OS path would pay a heap allocation per file operation on the hottest loops (status walks, realize), which G6 rules out. The provider itself is `dyn` (`Arc<dyn FilesystemProvider>`), because it is called once per command.

## Amendments

**2026-11-02.** The implementation took a different route through stages 2 to 4, and this text now describes it. The goals, the seam and the staging are unchanged.

- **No held-`State` hand-over.** The proposal had SWFS hold the working tree as a `State`, take whole target revisions through a `hold_state` call, and be told by `stored` and `committed` what each commit stored. SWFS does hold `State`s: the served tree, built from the current and staged revisions, the layers and the links, and the modified-file record. But the operation syncs through the ordinary realize path and reloads the served tree from the instance's current anchor at each thaw, so the `holds_state`, `hold_state`, `stored` and `committed` hooks, `HoldOptions` and `fs/hold.rs` are removed.
- **A modified-file record instead of `ChangedWhileFrozen`.** `ModifiedTree` holds the changes, fed by `notify_*` and rebuilt from `swfsGetModifiedFileList` ([ADR-00029](../developing/decisions/00029-swfs-modified-file-record.md)).
- **Commit names what it commits** (`committed_path`) in place of `stored`/`committed`, so SWFS drops its record of each committed change.
- **No refusals** of dependency-set syncs, sync resets, layers, or commits of a link or layer alone, because the ordinary paths cover them.
- **Links and layers** are served from their own repositories through the served tree, and described by mount path ([ADR-00034](../developing/decisions/00034-swfs-serve-links-and-layers.md)).
- **One test suite for both providers** runs as `--all-swfs` in `scripts/test/conftest.py`.
- **New constraints,** recorded under SWFS plumbing: no walk of the tree through the OS, no write of the revision's own changes through the OS, the eight-mount limit, the refusal of an operation inside another on one mount, and the service-only rule.
- **What goes through the mount** is a rule of the SWFS paths, not an optimization. A write through the mount is the user's change as far as SWFS can tell, so Lore describes what a revision holds to the thaw and writes through the mount only what is a user edit: scratch files and the results of merge, resolve, cherry-pick, `revert --no-commit` and a dependency sync's kept content. Stage 2 and stage 5 describe those writes as built.
- **Removing what the user holds** is done by naming it in the thaw. This works for files and not for directories the user made, which stay. See Risks and Assumptions.
- **Case-keeping stage** defers the rename to the commit's thaw.
- **SWFS behavior assumed.** The text assumes the SWFS contracts in `swfs.h`: a thaw changes a node's type or spelling in one step, a move lists its origin, and a thaw that names a changed path commits it. See Risks and Assumptions.

## Prior Art

- **VFS for Git** ([microsoft/VFSForGit](https://github.com/microsoft/VFSForGit)) virtualizes a Git working tree on Windows through ProjFS. It keeps a list of modified paths so that Git does not scan the whole tree, which is the same idea as stage 3's list-driven diff. It integrates largely beside Git's own working-tree code, through hooks and a read-object protocol, rather than through one seam inside Git. That is the drift this proposal avoids.
- **EdenFS** in Sapling ([facebook/sapling](https://github.com/facebook/sapling), `eden/fs`) serves a checkout virtually and answers status from its own record of modified files. Its checkout updates the presented tree to a target commit inside EdenFS, much as a sync here updates the tree the mount presents.
