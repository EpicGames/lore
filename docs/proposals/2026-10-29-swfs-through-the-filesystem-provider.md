---
lep: 2026-10-29-swfs-through-the-filesystem-provider
title: SWFS Support Through the FilesystemProvider Interface
authors:
  - matthew.hoffman
status: Accepted
created: 2026-10-29
updated: 2026-10-29
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
| `FilesystemProvider::holds_state`, `InstanceOperationImpl::hold_state`, `stored`, `committed` | Hooks for a provider that holds the working tree as a `State` rather than as files. Sync hands it a whole target revision. Commit tells it what each file was stored as and which revision became current. See stages 2–5. |

The rules that make two providers behave the same stay in the commands: filter folding, mode derivation (`FileInfo::mode`, `mode_differs_from`), conflict and diffability inference (default methods derived from `content_source`), and local-modification checks. The provider answers questions about the tree but never decides policy (G1). Where a provider can answer a derived question more cheaply, it overrides the default method (`names_folding_to`, `infer_is_diffable`, `infer_is_conflicted`). It does not get a separate code path.

### Phase 0 — the OS provider, as a series of no-op changes

This phase addresses G4 and G6. It ships before any SWFS behavior and changes nothing a user can observe.

1. **Introduce the traits and `OsFilesystem`/`OsOperation`** (`lore-revision/src/fs/os/mod.rs`). Each `OsOperation` method calls the same `lore-io` driver call the command made before. `RepositoryContext` gains a `file_system` field defaulting to `OsFilesystem::new(path)`, and server contexts get an empty-path provider. Nothing calls the new code yet.
2. **Move call sites one command at a time.** Each change wraps a command's working-tree access in `with_operation(repository.file_system(), …)` and replaces direct I/O with operation methods: `file/info.rs`, `file/hash.rs`, `file/dirty.rs`, `file/stage.rs`, `file/unstage.rs`, `file/reset.rs`, `file/write.rs`, `file/obliterate.rs`, `file/diff.rs`, `diff.rs`, `commit.rs`, `branch/merge.rs`, `revision/sync.rs` and `fs/realize.rs`. With only `OsOperation` behind the seam, each change is a refactor. The existing unit and smoke tests are the oracle, and a regression can be traced to one reviewable, revertible change.
3. **Lift the shared concerns into `InstanceOperationImpl`** (dry run, change tracking, modified times) as separate changes, each removing the duplicate handling from the command it replaces.
4. **Add a test provider** (`StaticDispatchInstanceOperation::Test`, `filesystem_provider::test_util`). Tests use it to assert operation boundaries: each command finalizes exactly once, reports `changes_made` correctly, and finalizes after a failure. They also check the `holds_state` refusals. See `lore-revision/tests/unit/fs/filesystem_provider.rs`.

Once phase 0 is done, every working-tree access is routed through one point that a second provider can occupy.

### SWFS plumbing shared by every stage

- **Configuration.** `RepositoryConfig.vfs: Option<VfsConfig>` with `VfsType::{None, Swfs}` (`lore-revision/src/repository.rs`). `RepositoryConfig::validate` refuses SWFS without a shared store, because the mount serves reads from the immutable store. This is also what lets several SWFS instances of one repository share one copy of each file's content: every mount reads from the same store, and only an instance's write directory holds anything of its own.
- **Where an instance lives.** An SWFS instance's `.lore` and its SWFS `write` directory live in the external instance directory (`external_dir_for_instance`), not inside the mount.
- **Mount lifecycle.** `MountManagerState` (`lore-revision/src/fs/mount_manager_state.rs`) holds one `MountManager` per service process. The service initializes it on startup (`lore/src/service/initialization.rs`) and remounts every SWFS instance it finds in the shared-store registry (`MountManager::mounts_of_entries`). Opening an SWFS instance outside the service is an error (`repository.rs`, "Loading a SWFS repository without using the service").
- **Serving reads.** `MountResources` (`lore-revision/src/fs/swfs/mount_resources.rs`) holds the mount's own `RepositoryContext` and the `State` it presents. `read_file` resolves the path to a node and reads the requested range from the immutable store. `enumerate_directory` lists a node's children with their sizes and recorded modification times.

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

- `SwfsFilesystem` answers `holds_state() == true`. The provider holds the working tree as a `State`, namely the current revision plus the local changes it serves, rather than as files.
- `sync` hands the target revision to the operation (`InstanceOperationImpl::hold_state`, `HoldOptions { overwrite_local, keep_local, dry_run }`). It does not realize the target file by file. `lore-revision/src/fs/hold.rs` provides the parts that stay common:
  - `hold::changes` diffs the current revision, read clean, against the target. A path left unsettled by an interrupted earlier apply is applied again.
  - `hold::check` refuses a change that would replace a local change unless `overwrite_local` or `keep_local` is set. This is the same rule a disk sync applies.
  - `hold::report` emits the same per-file sync events as realize.
- The SWFS provider applies the changes to the `State` it holds. On thaw it describes each changed path to SWFS with the modification flags that invalidate the cached entry (`SWFSNodeModFlag_*`).
- To know which paths carry local changes, the provider reads the list of files modified in the frozen mount (`swfsGetModifiedFileList`, `SWFSNodeModFlag_ChangedWhileFrozen`). Stage 2 needs that list. It does not need the content of those files.
- `sync` refuses three things when the provider holds state (`revision/sync.rs`): a sync restricted to a dependency set, a reset of local changes, and a sync that carries layers. Stage 2's hold applies whole revisions of the parent repository alone. It has no way yet to restrict a hold to some paths, to discard local changes, or to carry a layer's tree. These refusals are temporary: stage 4 lifts the layer refusal, and stage 5 lifts the other two.

What users get: `lore sync` on an SWFS instance, with the same local-change refusals and options as on disk.

### Stage 3 — commit

Goal: read the user's writes out of a frozen mount and commit them through the ordinary commit path.

- `changes_from_filesystem_to_state` answers from the modified-file list instead of walking. This is the optimization G2 exists for: `status`, `stage` and `commit` visit only what the user touched, and never hydrate an unmodified file.
- `content_source` and `file_holds_content` read modified files from the frozen mount (`swfsReadFrozenFile`). Unmodified files are answered from the held `State` without reading.
- Commit tells the operation what it stored for each file it read (`InstanceOperation::stored(path, address, size, mode)`). Once the revision is current and flushed, commit hands it over (`committed(repository, state)`). The provider then serves those files from the committed revision instead of from the write directory, and the thaw clears them from the modified set. A failed `committed` is logged and the commit stands. A dry run and a failed commit call `stored` but not `committed`.
- `commit` refuses a commit of a link alone or of a layer when the provider holds state (`commit.rs`), because either would make a revision current that the provider is never handed.

What users get: `status`, `stage`, `commit` and `reset` on an SWFS instance, with the same semantics as on disk.

### Stage 4 — links and layers

Goal: a mounted instance presents, syncs and commits its links and layers as an on-disk instance does, and the refusals that stages 2 and 3 put in place for them are lifted.

A link places another repository's tree at a path in the parent (`LinkMountInfo`), and a layer overlays another repository's files at a path (`LayerMountInfo`, `lore-revision/src/state.rs`). Each has its own `RepositoryContext` and `State`. On disk they share the parent's provider and operation: one provider covers the whole mounted filesystem (`FilesystemProvider::begin_operation` docs). Stage 4 keeps that shape. A link or layer stays a subtree of the parent's mount and is never a second mount.

- **Serving.** The read callbacks resolve a path across the link and layer mount points the presented tree holds. A path below a link reads its node from the linked repository's `State`, and its content from that repository's store. A path below a layer reads from the layer's `State` where the layer holds a node there.
- **Sync.** `hold_state` takes the target of each link and layer along with the parent's. `hold::changes` and `hold::check` run once per repository, so a local change under a link is refused, kept or overwritten under the same `HoldOptions` as one in the parent. The thaw describes changed paths under a mount point relative to the mount root, as it does for the parent. This lifts the refusal of "sync layers" (`revision/sync.rs`).
- **Commit.** `stored` and `committed` carry the repository each file belongs to, so the provider takes a link's or layer's committed revision for the subtree it covers. This lifts the refusals of "commit a link alone" and "commit a layer" (`commit.rs`). The provider is now handed every revision that a commit makes current.
- **Change detection.** The provider splits the modified-file list from stage 3 by mount point, so `changes_from_filesystem_to_state` answers each link and layer diff from the entries under its path. It does not walk the subtree.

What users get: repositories that use links and layers work on SWFS, with the same sync and commit behavior as on disk.

### Stage 5 — every command, and the edge cases

Goal: every `lore` command either works on an SWFS instance through what stages 1–4 built, or refuses with an error that names what it cannot do. The behavior differences that remain between the two providers are found and closed.

- **Command coverage.** Stage 5 checks each command in `docs/reference/lore-cli-commands.md` that reaches the working tree against an SWFS instance. The per-path commands, such as file reset, `obliterate`, file write and merge resolution with its `~base`/`~mine`/`~theirs` sidecars (`BASE_SUFFIX`, `MINE_SUFFIX`, `THEIRS_SUFFIX` in `lore-revision/src/repository.rs`), run on the fine-grained primitives in the interface. For a provider that holds state, those primitives update the held `State` and the write directory, rather than recording a path for the thaw alone.
- **The remaining refusals.** A sync restricted to a dependency set and a sync that resets local changes (`revision/sync.rs`) move onto `hold_state`. A reset is a hold that overwrites every local change, and it also discards those files from the write directory. A dependency-set sync is a hold of the target restricted to the set's paths. Once this stage is done, `fs::hold::refused` has no callers outside tests.
- **No OS fallbacks.** No `SwfsOperation` method delegates to `OsOperation` (`fake_with_os!` in `swfs/filesystem.rs`), except `untracked_file_info` and the writes of files the revision does not track, which really are disk files.
- **Edge cases**, each settled to match the OS provider or documented where it cannot:
  - case-only renames, and two spellings of one name, on a mount that folds case;
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
| G1 | The seam; policy stays in commands; `hold.rs` shared by stages 2 and 4; stage 5's shared test suite |
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
- **CLI and public API** — `lore repository create` and `lore repository clone` gain `--vfs <none|default|swfs>`, defaulting to `none` (`docs/reference/lore-cli-commands.md`). `default` currently resolves to SWFS (`LoreVfsType::to_config`). `--dry-run` with `--vfs swfs` is refused on clone. The stage-2 and stage-3 refusals (dependency-set sync, sync reset, layers, committing a link alone) apply only to SWFS instances and return `InvalidArguments` (`fs::hold::refused`). They are temporary: stage 4 lifts the link and layer refusals, and stage 5 lifts the rest. Nothing changes for instances created without `--vfs`.

## Non-Functional Considerations

- **Concurrency** — A filesystem holds one operation at a time, and the next begins only after that one is finalized (`FilesystemProvider::begin_operation` docs). For SWFS this means one freeze at a time per mount. Commands that only read the working tree, such as `status`, `diff` and file info, can ask for an operation while another is in progress. A future ADR will decide how to handle such concurrent reads ([ADR-00026](../developing/decisions/00026-concurrent-read-only-instance-operations.md), proposed). One option is for the second request to block until the operation in progress finalizes. The other is for concurrent readers to share one read-only `InstanceOperation`, and so one freeze. `finalize` can be called only once (`claim_finalize`), so a second caller cannot thaw a freeze that someone else still holds. SWFS read callbacks run concurrently with an operation and read the presented `State`, which changes only inside `thaw` (`update_state` under an `RwLock`).
- **Memory** — The OS path is unchanged. SWFS holds one deserialized `State` for each repository a mount presents, each of which pages nodes on demand. That is one `State` for the parent through stage 3, and from stage 4 one more for each link and layer, so the count grows with the number of links and layers, not with the size of the tree. On top of that it holds a touched-path set per operation that grows with the paths written, not with repository size. Stage 3's list-driven diff scales with the number of modified files, where a disk walk scales with the tree.
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
- **Assumption:** A freeze over the whole mount is cheap enough to take once per command. — *invalidated if:* freeze latency grows with tree size or open handles, which would make selective freezing a requirement and no longer an optimization.

**Risks**

- **Risk:** An `OsOperation` method diverges subtly from the direct I/O it replaced during phase 0, for example in error mapping or case handling. — *mitigation:* Most operations' implementation can be 1:1 copied from the code they replace due to existing code re-use. In edge cases the author's judgement can match up error cases, with the existing test suite as the oracle.
- **Risk:** A command reaches the working tree without going through the operation and bypasses SWFS. — *mitigation:* the contexts that must not touch a working tree carry `InvalidFilesystemProvider`.
- **Risk:** A thaw fails partway through, leaving SWFS presenting a state that differs from Lore's current anchor. — *mitigation:* `finalize` always clears the touched set and reports the failure. `hold::changes` re-applies paths an incomplete apply left unsettled on the next hold.
- **Risk:** Stage 1 instances run `status`/`stage`/`commit` through the OS-delegated reads before stage 3 lands, and get incorrect results. — *mitigation:* Explicitly inform early adopters about SWFS support and ensure it is only used as supported. The limited release of the EpicVFS driver limits early users to people in close contact with the Lore team.

## Drawbacks

- Every working-tree method now passes through a `match` over providers, and adding a provider means touching every forwarding arm in `InstanceOperationImpl`.
- The interface carries hooks (`holds_state`, `hold_state`, `stored`, `committed`) that do nothing for the OS provider, which makes the trait harder to learn.
- SWFS instances depend on the Lore service and a shared store, which on-disk instances do not.

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

## Prior Art

- **VFS for Git** ([microsoft/VFSForGit](https://github.com/microsoft/VFSForGit)) virtualizes a Git working tree on Windows through ProjFS. It keeps a list of modified paths so that Git does not scan the whole tree, which is the same idea as stage 3's list-driven diff. It integrates largely beside Git's own working-tree code, through hooks and a read-object protocol, rather than through one seam inside Git. That is the drift this proposal avoids.
- **EdenFS** in Sapling ([facebook/sapling](https://github.com/facebook/sapling), `eden/fs`) serves a checkout virtually and answers status from its own record of modified files. Its checkout updates the presented tree to a target commit inside EdenFS, much as stage 2's `hold_state` hands a target revision to the provider.
