---
lep: 2026-05-03-modified-file-tracking
title: Modified file tracking
authors:
  - Mattias Jansson
status: Draft
created: 2026-05-03
updated: 2026-05-03
discussion: https://crowd.urc.internal.epicgames.net/Epic/URC/change-request/new/main/mjansson%2Fmodified-file-tracking-impl
superseded-by: 2026-06-21-changesets
---

# Modified File Tracking

## Summary

This proposal adds a lightweight mechanism for tracking which files in a Lore repository have been modified on disk
without requiring a full filesystem scan. A new orthogonal `Dirty` flag on merkle tree nodes allows external
applications and the Lore CLI to mark files as changed, and a new `--scan` flag on `lore status` triggers a filesystem
crawl that persistently records which files differ from the committed revision.

## Motivation

Lore has no way to track which files have been modified between commits. The only mechanism to discover filesystem
changes is `lore status --unstaged`, which triggers a full recursive scan comparing content hashes against the committed
revision. In large repositories this takes seconds to minutes, making it impractical for interactive use.

External applications - IDEs, file system watchers, build systems, VFS providers - already know which files have
changed through OS-level change events. But they have no way to communicate this to Lore. The only option is to stage
the file (declaring intent to commit) or do nothing and wait for a full scan.

This creates three problems:

1. **Status is blind by default.** `lore status` shows only staged changes. Seeing local modifications requires an
   explicit `--unstaged` scan that is too slow for routine use in large repositories.

2. **No notification path.** There is no intermediate state between "unchanged" and "staged for commit." Integrations
   must either over-commit (stage everything) or under-report (ignore changes until the user scans).

3. **Duplicated tracking.** Every application that needs modification state must implement its own tracking. These
   independent implementations diverge in behavior, miss different edge cases, and cannot share state with each other
   or with Lore.

The current `--unstaged` flag conflates scanning the filesystem (expensive I/O) with displaying known changes (cheap
state read). Separating these would let a scan persist its results for instant subsequent display.

This gap becomes more acute with virtual file system integration. A VFS provider receives write, rename, and delete
callbacks from the kernel but has no way to feed this knowledge into Lore, forcing a crawl of the potentially sparse
working directory and negating the performance advantage of virtual file systems.

## Goals / Non-Goals

### Goals

1. **Track modified files as a first-class repository concept.** Lore should know which files have been modified on disk
   without requiring a filesystem scan, and this knowledge should persist across operations.

2. **Enable external integrations to report modifications.** Applications, file system watchers, and VFS providers
   should be able to tell Lore about file changes through a lightweight API, without staging.

3. **Show modifications without scanning.** Users should see locally modified files in `lore status` by default, without
   an explicit flag or a full filesystem crawl.

4. **Separate scanning from display.** On-demand filesystem scanning should persist its results so that subsequent
   status calls can display known changes instantly.

5. **Maintain modification state across the staging and commit lifecycle.** Staging a modified file should not lose the
   modification tracking. Only committing a file should clear its modification state. Files not included in a commit
   should retain their modification state.

### Non-Goals

- **Content-level change tracking.** This proposal tracks which files changed, not what changed within them.
- **Change detection.** This proposal provides the tracking facility, not the detection mechanism. File system
  monitoring, VFS provider integration, and IDE change detection are integration concerns - the dirty tracking API
  serves as their common target.
- **Separate action tracking per lifecycle state.** Dirty and staged share a single action type per node. Tracking
  independent actions (e.g., "staged as modify but dirty as delete") is out of scope.

## Proposed Design

Lore takes ownership of modification tracking as a first-class concern in the repository state. The proposal separates
the detection of modifications (the integration's responsibility) from the tracking of modifications (Lore's
responsibility). External integrations detect changes and push them into Lore through a lightweight API; Lore owns the
persistent state, tree propagation, and operation interactions. All consumers read from one authoritative source.

### Dirty flag in the merkle tree

Add a `Dirty` flag at bit 3 of `NodeFlags` (V1 u16) and bit 15 of `NodeFlagsV2` (V2 u32). These positions are
currently unused (`Unused1` in V1, undefined in V2). The existing V2-to-V1 conversion handles the new flag as a direct
bit mapping, following the same pattern used for `File`, `Module`, and `Discarded`.

Dirty and Staged are orthogonal flags that share the existing action bits (Modify, Add, Delete, Move, Copy at bits 5-9
in V1). A node has a single action type regardless of lifecycle state. The four valid states for a changed node are:
dirty only, staged only, dirty+staged, and clean.

The dirty flag propagates up the merkle tree to parent directories with an early-out optimization (stop if the parent is
already dirty), mirroring the existing staged flag propagation in `node_mark()`. A parallel `node_mark_dirty()` function
and `node_has_dirty_children()` query function provide the propagation and inspection primitives.

This addresses **Goal 1**.

### File dirty API

A new `lore file dirty` command marks files as dirty in the staged state anchor:

- `lore file dirty <paths>` - checks filesystem existence (not content) to classify the action: modify (file exists and
  is in current revision), add (file exists but not in revision), delete (file missing but in revision), or ignore (file
  missing and not in revision). For directories, recurses into children.
- `lore file dirty move <from> <to>` - marks a file as moved without filesystem verification (caller-trusted).
- `lore file dirty copy <from> <to>` - marks a file as copied without filesystem verification.
- `lore file dirty --targets <file>` - bulk operation from a targets file.

The API respects the same ignore and view filters as the stage API. Dirty state is persisted in the existing staged
anchor - no new anchor type is introduced.

A `lore dirty` top-level shortcut mirrors the existing `lore stage` shortcut.

The `file dirty` API serves as the integration point for file system watchers and virtual file system providers. A VFS
provider that intercepts write, rename, and delete callbacks from the kernel can translate each callback into a
corresponding `file dirty` call, maintaining an accurate dirty state without any filesystem scanning. The same API
serves IDE integrations, build systems, and any other process that observes file changes.

This addresses **Goal 2**.

### Status display

`lore status` now displays dirty files by default in a "Dirty files (not staged for commit)" section, between the
staged section and the (now legacy) unstaged section. Dirty+staged files appear in the staged section. A new
`flag_dirty` field in the `LoreRepositoryStatusFileEventData` C struct and JSON event identifies dirty files in
programmatic output.

The diff engine (`diff_subtree_node()`) is extended to detect dirty-flagged nodes even when their content hash matches
the comparison state, ensuring dirty files surface in the diff output that drives status display.

This addresses **Goal 3**.

### Scan flag

A new `--scan` flag on `lore status` triggers a filesystem crawl using the existing `diff_filesystem()` infrastructure.
The scan compares the filesystem against the staged state (whose content hashes equal the committed revision's hashes,
since staging and dirty marking do not update content). The `diff_filesystem` walker is extended with a `scan_dirty`
mode that sets Dirty on modified files and clears stale Dirty on retained (matching) files inline during the walk, with
zero additional tree traversal cost. Parent directory cleanup (clearing Dirty on directories with no remaining dirty
children) happens inline at the retain point.

`--unstaged` becomes a hidden alias for `--scan`. The internal `StatusOptions.unstaged` field is renamed to `scan`.

This addresses **Goal 4**.

### Operation interactions

- **Stage** adds the Staged flag without clearing Dirty. Flag-clearing operations use helper functions that preserve
  Dirty and action bits when Dirty is set.
- **Unstage** clears Staged, then re-checks the filesystem: clears Dirty if the file matches the committed revision,
  preserves Dirty if it still differs. Parent directory cleanup walks up the tree, clearing Dirty on directories with
  no remaining dirty children. The staged anchor is preserved if dirty-only nodes remain after unstaging.
- **Reset** on a dirty-only file restores the file content and clears Dirty with parent cleanup. Reset on staged or
  dirty+staged files continues to refuse (unchanged behavior).
- **Commit** clears both Dirty and Staged on committed files. Dirty-only files are preserved across the commit. After
  commit, if dirty-only nodes remain, the staged state is re-parented to the new revision and persisted as the new
  staged anchor. If no dirty nodes remain, the anchor is deleted.
- **Sync** does not delete the staged anchor. Dirty flags survive sync to a new revision.
- **Branch switch** preserves the staged anchor (and dirty flags) on a normal switch. A forced switch (`--reset`)
  deletes the staged anchor, clearing all dirty state.
- **Merge, cherry-pick, revert** preserve existing flags via additive flag operations. Dirty flags survive these
  operations.

This addresses **Goal 5**.

## Compatibility

- **Wire format** - N/A. Dirty flags exist only in the local staged anchor, which is never transmitted to remote peers.

- **Client/server protocols** - N/A. The `file dirty` API and `--scan` flag are local-only operations. No new RPCs or
  protocol changes.

- **On-disk format** - The `Dirty` flag occupies bit 3 of `NodeFlags` (previously `Unused1`) and bit 15 of
  `NodeFlagsV2` (previously undefined). Both bits were reserved and unused. An older Lore client reading a staged state
  written by a newer client will see the dirty flag as an unknown bit in the flags field. Since older clients do not
  interpret bit 3 and the bit is outside the `StagedBits` mask (bits 4-14), it will not affect staged flag
  interpretation. The dirty bit will be silently preserved through read/write cycles by older clients because node
  flag fields are read and written as opaque integers. The dirty bit is only meaningful in the local staged anchor,
  which is never shared with other clients.

- **CLI and public API** - New commands: `lore file dirty <paths>`, `lore file dirty move <from> <to>`,
  `lore file dirty copy <from> <to>`, `lore dirty` (shortcut). New flag: `lore status --scan`. The `--unstaged` flag
  is retained as a hidden alias. New field `flag_dirty` appended to the `lore_repository_status_file_event_data_t`
  C struct after `flag_conflict_theirs` to preserve FFI layout compatibility for existing fields. Consumers that
  previously identified unstaged files by checking `flag_staged == false` in scan/unstaged events should migrate to
  checking `flag_dirty == true`.

## Non-Functional Considerations

- **Concurrency** - The `file dirty` API operates on the staged anchor, which is a single-writer resource (same
  concurrency model as `stage` and `unstage`). Concurrent dirty notifications from multiple processes are not safe
  without external coordination, matching the existing model for all staging operations.

- **Memory** - Dirty flags are stored as single bits on existing merkle tree nodes. No additional data structures are
  allocated. The `diff_filesystem` scan with `scan_dirty` mode sets and clears flags inline during the walk with no
  buffering beyond what the existing diff infrastructure uses. The `collect_dirty_file_nodes()` helper walks only dirty
  subtrees (directories without the Dirty flag are skipped).

- **Statelessness** - Dirty state is persisted in the staged anchor, which is a file on disk. No in-process state
  survives across operations beyond the standard state deserialization/serialization cycle.

- **Determinism** - The same filesystem state produces the same dirty flags. The `file dirty` API is deterministic
  given the same inputs (paths + filesystem existence). The `--scan` flag is deterministic given the same filesystem
  state and committed revision.

## Migration Plan

N/A - no breaking changes, no migration required. The `--unstaged` CLI flag is preserved as a hidden alias for scan.
The new `flag_dirty` field is appended to the C struct, preserving layout for existing fields.

## Security Considerations

This proposal has no security implications. Dirty flags are local-only metadata stored in the staged anchor, which is
never transmitted to remote peers or included in committed revisions. The `file dirty` API does not read file content
— it checks only filesystem existence for action classification. The trust model is unchanged: the local user has full
access to their own repository state.

## Privacy Considerations

This proposal has no privacy implications. Dirty flags record file paths (already part of the repository tree) and
action types (modify, add, delete, move, copy). No new user identifiers or metadata are introduced. Dirty state is
local-only and never leaves the client machine.

## Risks and Assumptions

**Assumptions**

- **Assumption:** Bit 3 of `NodeFlags` (V1) and bit 15 of `NodeFlagsV2` (V2) are unused in all deployed clients.
  *Invalidated if:* a deployed client uses these bits for an undocumented purpose. The V1 bit is explicitly named
  `Unused1` in the current codebase.

- **Assumption:** External applications (IDEs, file watchers) can detect filesystem changes with sufficient accuracy
  to make the `file dirty` API useful without content verification. *Invalidated if:* OS-level change notifications
  are unreliable enough that dirty state becomes chronically stale, defeating the purpose of avoiding full scans.

**Risks**

- **Risk:** Shared action bits between Dirty and Staged mean that a `file dirty` call can alter the action type
  previously set by a stage operation. For example, staging a modify and then marking the file as dirty-deleted changes
  the staged action to delete. *Mitigation:* This is the documented "latest action wins" semantic. The action reflects
  current filesystem reality, not historical intent. Users who stage a file and then delete it on disk should expect
  the action to reflect the deletion.

- **Risk:** Scan with `scan_dirty` mode modifies the staged state during a read-oriented operation (`lore status`).
  If the scan is interrupted (crash, signal), the staged state may be partially updated. *Mitigation:* The staged
  anchor is updated atomically after the full scan completes. A crash during the scan leaves the previous anchor
  intact. Partial dirty state from `file dirty` calls (which persist immediately) is no worse than partial staged
  state from an interrupted `lore stage`.

## Drawbacks

- Dirty flags share action bits with Staged, preventing representation of simultaneous but different actions for the
  two lifecycle states (e.g., "staged as modify, dirty as delete").
- The `file dirty` API checks filesystem existence to classify actions, which introduces a synchronous filesystem call
  that could race with rapid file system changes in the interval between the caller's detection and Lore's existence
  check. The explicit `move`/`copy` subcommands avoid this by trusting the caller entirely.
- The staged anchor grows with the number of dirty-tracked files, since dirty state is persisted alongside staged state
  in the same merkle tree. This is bounded by the total number of files in the repository and uses the same block-based
  serialization as staged state, so the cost scales with the existing infrastructure.

## Alternatives Considered

### Separate dirty anchor

Store dirty state in a separate anchor (`anchor-dirty`) with its own serialized state, independent of the staged
anchor.

*Rejected because:* This doubles the serialization/deserialization cost on every status call (two state trees to load
and diff). The staged anchor already handles state persistence, and dirty flags fit naturally as additional bits on the
same nodes. The single-anchor model avoids synchronization between two state trees and reuses all existing infrastructure.

### Flat path-based dirty set

Store dirty paths in a separate flat data structure (hash set or sorted list) outside the merkle tree.

*Rejected because:* This loses integration with the tree traversal and diff infrastructure. Path-based lookup cannot
leverage the merkle tree's hierarchical structure for efficient subtree operations (is any file under this directory
dirty?). It would also require reconciling the flat set with the tree on every status call, adding complexity without
performance benefit.

### Content hashing during `file dirty`

Have the `file dirty` API read and hash file content to verify the file actually changed, rather than trusting the
caller or checking only existence.

*Rejected because:* Content hashing defeats the purpose of a lightweight notification path. The caller (IDE, file
watcher) already knows the file changed. Reading and hashing the file re-introduces the I/O cost that the dirty
tracking API is designed to avoid. The `--scan` flag provides content-aware verification when the user wants it.

## Prior Art

- **Git** uses the index (staging area) to track the working tree state. `git status` compares the working tree against
  the index and the index against HEAD, performing a full filesystem scan each time. Git mitigates the scan cost with
  the filesystem monitor (fsmonitor) extension, which hooks into OS-level change notifications (FSEvents on macOS,
  ReadDirectoryChangesW on Windows) to avoid scanning unmodified subtrees. Git's VFS for Git (formerly GVFS) integration
  maintains a modified-paths list that the projection file system provider populates with change events. Lore's
  `file dirty` API serves a similar role - external processes and VFS providers report changes - but integrates the
  result into the existing merkle tree rather than using a separate watch bitmap or side file.

- **Perforce** tracks file state through explicit checkout. Files are read-only until the user runs `p4 edit`, which
  both marks the file as open for edit and makes it writable. This is the opposite extreme: no implicit change
  detection at all. Lore's approach sits between Git's implicit scanning and Perforce's explicit checkout by allowing
  both explicit notification (`file dirty`) and on-demand scanning (`--scan`).

- **Mercurial** uses a dirstate that records the expected size, mtime, and mode of each tracked file. `hg status`
  compares filesystem metadata against the dirstate to identify changes without reading content, falling back to
  content comparison only when metadata differs. Lore's `is_file_modified()` function uses the same size+mtime
  fast path. The Mercurial Rust rewrite (rhg) added a filesystem watcher integration similar in spirit to Git's
  fsmonitor.

## Unresolved Questions

- Should the `file dirty` API verify content against the committed revision when called without an explicit action
  subcommand, or is filesystem existence checking sufficient for action classification?
- How should `--scan` handle dirty-moved nodes where the destination file is missing? Options include reverting the
  move in the tree (restoring the node to its committed position) or converting to a dirty-delete.
- How should `--scan` handle dirty-added nodes (zero content hash) where the file has been deleted from disk? Options
  include discarding the node entirely (if not staged) or clearing only the dirty flag (if staged).
