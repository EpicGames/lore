---
lep: 2026-10-31-copy-on-write-instance-clone
title: Copy-on-write instance clone
authors:
  - Zack Neyland
status: Draft
created: 2026-10-31
updated: 2026-11-10
---

# Copy-on-write instance clone

## Summary

This proposal lets one machine hold many working trees of a very large repository at little extra disk
cost. `lore repository instance clone <destination>` copies an instance to a new path as a copy-on-write
clone, so the copy shares the source's data blocks, and registers the copy as an instance of its own on the
same revision. Where `lore clone` makes an instance from a remote, an instance clone makes one from an
instance already on the machine. The instance's filesystem provider decides how the clone is made: it answers,
before anything is written, whether a clone between two paths would share blocks, copy bytes or is
unsupported, and it performs the clone through one clone backend per filesystem, so a new filesystem, OS or
virtual provider adds one backend. The clone refuses when it cannot share blocks, unless the user opts into a
full copy with `--allow-copy`. It depends on
[instance-local modified-time records](2026-11-03-instance-local-modified-time-records.md), so the copy
starts with valid records, and on [resumable sync](2026-11-03-resumable-sync.md), whose record
of an unfinished sync tells the clone when the source is mid-sync.

## Motivation

The tree measured for this proposal, a partial view of a large game project, holds 456 GB in 1,085,758
files and 5.0 million entries (about 3.9 million of them directories). A typical full sync of that project
is about 1 TB; Risks and Assumptions states that workload as an assumption. Developers sync it on Mac
and Windows, Windows being the majority, and build machines sync it too. Each task that needs its own edits and builds, for a person or an agent, needs a working tree
of its own.

- **Every full instance costs a full tree on disk.** A second instance over the shared store
  ([Run parallel instances over a shared store](../how-to/run-parallel-instances-over-a-shared-store.md))
  avoids downloading again, but a sync writes every file its view selects as a file of its own
  (`lore-revision/src/fs/realize.rs`). A sparse view makes that smaller, but a build needs most of the tree,
  so each such instance adds 456 GB to 1 TB, and a handful fill a workstation's disk.
- **There is no way to copy an instance.** A copy made with the filesystem's own cloning
  ([`cp -c`](https://keith.github.io/xcode-man-pages/cp.1.html) on macOS,
  [`cp --reflink`](https://man7.org/linux/man-pages/man1/cp.1.html) on Linux) shares blocks, but it carries
  the source's instance ID in `.lore/instance` (see Repository instance in the [glossary](../glossary.md)) and
  has no registration, so Lore treats it as the source.
- **A copied directory hashes every file on its first scan,** because its modified-time records name the
  source's instance ID, so none apply to the copy. On the measured tree that is a read of about 456 GB.
- **What block cloning costs depends on the filesystem, not the OS.** APFS, Btrfs, XFS, bcachefs, ReFS and
  Dev Drive share blocks; NTFS and ext4 do not, and no filesystem shares blocks across volumes. One Windows
  machine can hold an NTFS system drive and a Dev Drive side by side. Where a clone cannot share blocks it is
  a full copy of up to 1 TB, which costs hours rather than minutes.

### How the figures were measured

All measured figures come from one machine: an Apple M3 Max (Mac15,9) with 64 GB of memory and its internal
SSD, an APFS volume, macOS 26.6. The tree is the partial view above, with a built C++ editor in it. Lore is a
release build of a prototype of this design.

- Times are wall-clock times from `/usr/bin/time -p`.
- Disk use is the volume's free space from `df` before and after, since `du` counts cloned blocks once for
  each file that shares them.
- The clone range covers the 17 clones of the tree recorded over three days. The scan range covers three
  clones.

Nothing was measured on Windows, on Linux, or on a full ~1 TB tree. Figures for those are estimates, labelled
as such where they appear, with their basis; Unresolved Questions lists the measurements that would confirm
them.

## Goals / Non-Goals

### Goals

- On a filesystem that shares blocks, clone an instance in a time and disk cost independent of its content
  bytes.
- The clone's first `status --scan` is clean and hashes nothing it cloned.
- Keep per-filesystem behaviour behind one interface: the instance's filesystem provider answers what a clone
  would cost before it writes anything, and adding a filesystem or OS means adding one clone backend. A
  virtual instance's provider supplies its own.
- Never make a full copy the user did not ask for: a clone that cannot share blocks refuses, and a copy is
  opt-in, with its size and lock hold stated up front.
- Work on every platform Lore supports, Windows included: sharing blocks on the filesystems that clone,
  copying on request elsewhere.

### Non-Goals

- Making a clone virtual, with files that materialize on first access. A clone of a virtual instance is in
  scope once its provider can clone; see Virtual instances.
- Sharing build outputs between clones. Build tools decide that; a clone carries what the source had.
- Keeping a clone sharing blocks with its source as either syncs forward. See What a clone shares over time.
- **Cloning an instance with a local store, for now.** The clone refuses these, and leaves their support for a
  proposal of its own. They do not simply work, even though the store's content is addressed relative to the
  instance: the clone writes the new instance's anchors and registration to the store the two instances
  share, and anchor keys include the instance ID (`lore-revision/src/instance.rs`). For a local store, that
  store is the source's own, so the writes land in the source's store and the cloned store never sees them.
  The store is also flushed only after the copy, so dirty mutable-store data would be missing from the cloned
  store. Supporting them needs these steps, in order: flush the source's store before the copy; open the
  clone's copied stores at the in-progress path; write the clone's anchors and registration there; and drop the
  source's registration and other per-instance keys from the copied store.

## Proposed Design

### Instance clone

`lore repository instance clone <destination> [--allow-copy]` (C API `lore_repository_instance_clone`),
next to `instance list`, `move`, `prune` and `delete`
([instance move and delete](2026-10-19-instance-move-and-delete.md)):

- **Asks the source's filesystem provider first** whether a clone to the destination would share blocks,
  copy bytes or is unsupported (see The instance's filesystem provider), before it takes any lock or writes
  anything.
- **Refuses when the clone cannot share blocks**, unless `--allow-copy` is given (see Copy is opt-in).
- **Requires a shared store.** An instance with a local store is refused before anything is written (see
  Non-Goals). With a shared store, `.lore/` holds no store data, so there is no store to flush or duplicate.
- **Clones the instance's directory**, `.lore/` included, through the backend the provider chose. File
  modification times, permissions and symbolic links are kept as they are. Directory times are not, since no
  scan reads them, and keeping them would cost a walk of every directory after a whole-directory clone; the
  clone never follows a link out of the tree. A repository nested in the tree, one holding a `.lore` directory,
  is an instance of its own, which stage and reset leave alone, so the clone leaves it out: copied, it would
  carry that instance's ID, and two trees would share its anchors. An older `.urc` directory is left out too,
  with the files beside it kept, as stage keeps them. A backend that clones whole directories first finds the
  nested repositories and the volumes mounted in the tree, whose files a whole-directory clone would not copy,
  and clones the directories above them, and each mount point, entry by entry. (Goal 1, Goal 5.)
- **Gives the copy its own identity:** a new instance ID, the source's anchors (current revision, branch and
  staged state), and a registration at the destination. The copy carries its new ID before any of the source's
  files is copied, so no part of it ever names the source.
- **Carries the instance's state with the directory.** The modified-time records in `.lore/records/`, which
  the clone writes back to disk before it copies `.lore/`, so the first scan hashes nothing they cover (Goal 2);
  records an older client left in the store name the source's ID and do not reach the clone, so the files only
  they cover are hashed once; the view, since the copy holds the same files; the instance's configuration;
  and the instance ignore rules of the
  [instance ignore rules](2026-11-03-instance-ignore-rules.md) proposal.
- **Leaves out the Lore directory's transient state:** lock files, a running service's state, and
  temporary files of interrupted writes. None of it belongs to the clone, and a copied lock would describe an
  operation the clone never ran. The content cache of [sync prefetch](2026-11-04-sync-prefetch.md) sits next
  to the store, outside every instance, so the clone never copies it; the record of an unfinished sync and
  its landed-file log never reach the clone, since the clone refuses a source that has them.
- **Holds the source and its store for the whole clone.** It takes the source's repository lock, and under
  [ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md) with it
  the shared store's locks, from the first write to the registration. It also keeps out other commands in its
  own process, such as the Lore service, which otherwise share one acquisition of that lock
  (`REPOSITORY_LOCK_CACHE` in `lore-revision/src/repository.rs`). So no command that takes these locks changes
  the source, and no sync starts in it, while it is cloned. `instance move` renames the source before it takes
  any lock, so it can change the source mid-clone: on Unix the move makes the clone fail, and on Windows the
  clone's open handles in the source refuse the rename. `instance delete` deregisters the source before it
  removes any file, and the deregistration takes these locks, so a delete waits for the clone. See Lock hold.
- **Writes into an in-progress directory** beside the destination, gives the copy the source's anchors,
  renames the directory to the destination, and then registers it. No registration names the destination
  before the clone is there; see Interrupted clones.
- **Refuses** a destination that exists, lies inside an instance or has no parent directory, a source with an
  unfinished sync (by the record resumable sync keeps, once that proposal lands), and a source on a local
  store.
- **Honours the global `--dry-run` flag** by running the checks that need no lock, printing the
  provider's answer (shares blocks, or copies N GB in M files, or unsupported and why) and stopping; a dry
  run that would copy refuses as the clone would, unless `--allow-copy` is given. The command reports
  progress as `RepositoryInstanceClone` begin, progress and complete events, as delete does.

Measured on the tree above, on APFS: 86 to 107 s and 1.2 to 1.6 GB of disk per clone. The disk is filesystem
metadata for the clone's own 5.0 million inodes and directory entries, about 280 bytes per entry; no file
content is copied. The clone's first `status --scan` took 105 to 171 s over three clones and reported only the
source's own uncommitted edits, where hashing the tree would read about 456 GB.

### The instance's filesystem provider

Lore already reaches an instance's files through its filesystem provider (`FilesystemProvider` in
`lore-revision/src/fs/filesystem_provider.rs`), with an OS provider for on-disk instances and an SWFS provider
for virtual ones. This proposal gives the provider two responsibilities for a clone (Goal 3):

- **Answer what a clone would do, before anything is written.** Given a source instance and a destination,
  the provider answers one of:
  - *shares blocks*: the clone costs metadata, not content;
  - *copies bytes*: the clone writes N bytes in M files, with the free space at the destination;
  - *unsupported*: no clone is possible, with a reason naming the filesystem or provider.
- **Perform the clone** it described, and report what it did: how many entries it cloned and how many it
  copied.

[Sync prefetch](2026-11-04-sync-prefetch.md) reuses this interface and its clone backends to place
individual files from its cache, and adds the questions it needs; this proposal does not depend on it.

A provider that does not implement these answers *unsupported*, so a provider that predates this proposal,
or one that cannot yet clone, makes the clone refuse rather than guess.

#### Clone backends

The OS provider does its part through clone backends, one per filesystem rather than per OS:

- **APFS** (macOS): [`clonefile(2)`](https://keith.github.io/xcode-man-pages/clonefile.2.html), which shares
  every block of a file or directory and fails rather than copies when it cannot.
- **Reflink** (Linux: Btrfs, XFS, bcachefs):
  [`FICLONE`](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html) per file, which shares the file's
  extents and fails with an error, not a copy, when the filesystem cannot.
- **ReFS and Dev Drive** (Windows):
  [`FSCTL_DUPLICATE_EXTENTS_TO_FILE`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_duplicate_extents_to_file),
  ReFS [block cloning](https://learn.microsoft.com/en-us/windows-server/storage/refs/block-cloning), which
  reports whether it cloned. The backend does not rely on
  [`CopyFileEx`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-copyfileexw) to clone:
  it block-clones only on Windows 11 24H2 and Windows Server 2025, falls back to a byte copy silently
  elsewhere, and does not say which it did.
- **Copy**: a plain byte copy, for every other filesystem (NTFS, ext4, network shares) and for a destination
  on another volume. It answers *copies bytes*, and an instance clone runs it only with `--allow-copy`.

A backend knows filesystems, not Lore: it clones and copies paths and reports what it did, and holds no
instance IDs, anchors or locks. The provider owns identity, exclusions and locking. Adding a filesystem or an
OS means adding one backend.

**Detection is per pair of directories.** For a source and a destination, the OS provider probes for real:
it writes a small file in the source's Lore directory, clones it into the destination's parent with each
candidate backend, and removes both. Only volumes of two different filesystems get the copy backend without
a probe. Volume identifiers (`st_dev` on Unix, the volume serial number on Windows) decide nothing: Btrfs
gives each subvolume its own `st_dev` although `FICLONE` works between them, and two copies of one virtual
disk can carry the same serial number. The first backend whose probe shares blocks
is the answer; if none does, the answer is *copies bytes*. A probe that fails for another reason, such as no
permission to write in the Lore directory, fails the clone. The probe, not the OS or the filesystem's name,
decides, so a Windows machine with an NTFS system drive and a Dev Drive gets the right answer for each, and a
filesystem mounted without reflink support gets *copies bytes*.

**A promise of sharing holds for the whole clone.** If the backend chosen for *shares blocks* fails to clone
an entry partway through, the clone stops and fails, rather than copying that entry, unless `--allow-copy`
was given. The prototype behind the measurements picks its implementation by OS at build time, finds out
one entry at a time whether it cloned, and falls back to a copy silently; this design moves the decision to
the probe.

#### Virtual instances

A virtual instance's provider supplies its own clone. The OS provider's backends do not apply, since a
virtual instance's tree is not on disk, and Lore never copies a virtual instance file by file. Replacing the
prototype's refusal of virtual instances by name, the clone asks the provider like any other.

Sketch for SWFS ([SWFS through the filesystem provider](2026-10-29-swfs-through-the-filesystem-provider.md)):
an SWFS instance keeps its `.lore` and its write overlay in its external instance directory, and the mount
presents the revision's tree from the store plus that overlay. SWFS would clone the external directory, its
Lore directory and write overlay, under the new instance ID, using the OS provider's clone backend for the
volume that holds it, and then mount the new instance at the destination path. The tree itself is not
copied, since it comes from the store, so the cost is proportional to the overlay, not to the tree. SWFS
answers *shares blocks* or *copies bytes* by the backend for its external directory's volume, with the
overlay's size.

Until SWFS implements this, its provider answers *unsupported* ("this virtual filesystem provider cannot
clone instances yet") and the clone refuses. (Goal 3, Goal 5.)

### Copy is opt-in

A full copy of a 1 TB tree takes hours and holds the store's locks throughout, so the clone never makes one
unasked (Goal 4).

- **Without `--allow-copy`**, a clone whose answer is *copies bytes* or *unsupported* refuses before writing
  anything. The message names the source's and destination's filesystems and volumes and the remedy: put
  the instances on a filesystem that clones (Dev Drive or ReFS on Windows, APFS on macOS, Btrfs, XFS or
  bcachefs on Linux) and the destination on the same volume as the source. For *copies bytes*, it also says what
  `--allow-copy` would cost.
- **With `--allow-copy`**, the clone states before it starts that it will copy N GB in M files and hold the
  store's locks throughout, so Lore commands wait for it as Lock hold describes. It
  refuses when the destination's free space is below the bytes it would copy.
- When the backend shares blocks, `--allow-copy` only lets the clone copy the entries that turn out not to
  clone. It does nothing for *unsupported*, which always refuses.

### Lock hold

The clone holds the source's repository lock and the store's locks for its whole length, so no other
command can start a sync in the source mid-clone or change the anchors the clone copies. Commands from other
processes on any instance sharing the store wait for the clone to finish (the command path takes the
repository's locks on entry, `lore/src/call.rs`). In the clone's own process, such as the Lore service,
the clone first waits for the commands already holding the source's repository lock, and while it waits no
more join them, so a stream of overlapping commands cannot keep it out; then commands on the source wait
too, since the clone holds the repository lock alone there; commands on other
instances share the process's hold of the store's locks and run. `instance delete` takes the
source's repository lock to deregister it, before it removes any file, so it waits too. `instance move` renames
the directory before it takes any lock. On Unix, one that moves the source during the clone makes the clone
fail, and then waits for the clone to end to record the new path; on Windows the clone's open handles in the
source refuse the rename.

How long that is depends on the backend. Only the APFS row on the measured tree is measured; every other
figure is an **estimate**. The ~1 TB tree is assumed to hold about 11 million entries, about 2.4 million
of them files and 8.6 million directories, scaling the measured tree's entries and files by 2.2, the ratio
of its content.

| Backend | Measured tree (5.0M entries, 456 GB) | Estimate at ~1 TB, ~11M entries | Basis of the estimate |
| --- | --- | --- | --- |
| APFS | 86 to 107 s, 1.2 to 1.6 GB (measured) | about 3 to 4 min, 2.6 to 3.5 GB | measured 17 to 21 µs and 240 to 320 B per entry, scaled by entries |
| Reflink (Btrfs, XFS, bcachefs) | not measured | not estimated | no measurement on Linux |
| ReFS / Dev Drive | not measured | tens of minutes (about 25 to 55 min) | assumed 0.3 to 1 ms per file to open, duplicate extents and close, plus about 8.6M directory creations |
| Copy (`--allow-copy`) | not measured | hours (about 1 to 3 h), 1 TB of free space | writing ~1 TB at a disk's sustained write rate, plus per-file creation and Defender reading each new file on Windows |

The block-cloning backends hold the locks for minutes; a copy holds them for hours, during which the store is
unusable to every other instance on the machine. That is why a copy is opt-in and states its cost first.

### What a clone shares over time

A clone shares blocks with its source only for files neither has rewritten since. Each file a sync or a build
writes into either instance is a private copy, so a clone that syncs forward, or builds, stops sharing those
files, and a clone that tracks a fast-moving branch converges towards a full copy. Deleting the source frees
nothing the clone still shares. Measure a clone's real cost with the volume's free space (`df`); `du` counts
each shared block once per file and overstates it. A clone therefore suits workspaces that diverge little
from their source or live briefly: an agent's task, a bisect, a review, a one-off build.

### Interrupted clones

The clone writes into an in-progress directory beside the destination, `<destination name>.cloning-<new
instance ID>`. Its Lore directory is marked as a clone in progress before anything is copied, and it has no
registration, so Lore never treats it as an instance. On success the clone stores the anchors and syncs them,
renames the directory to the destination, removes the mark and registers the instance. A failure before the
rename removes the directory after the command, in the background, since nothing else writes there and
removing millions of entries takes minutes; a cancelled copy keeps its locks until its work stops. A crash or
power loss before the rename leaves the directory and the clone's anchor keys, with no registration. A later
clone to the same destination removes a directory with that name only when it carries the mark, so it removes
only what a clone wrote, and a removal cut short is finished by the next clone; it leaves anything else beside
the destination alone. A crash after the rename leaves a working instance that registers itself when a write
command next opens it.

## Compatibility

- **Wire format** — N/A: no message or fragment changes.
- **Client/server protocols** — N/A for the server, which is unchanged. The client-to-service IPC gains one
  command, `RepositoryInstanceClone`, carrying the destination and whether a copy is allowed, appended after
  `RepositoryInstanceMove`, the last variant today, since bitcode identifies a variant by its position
  ([ADR-00022](../developing/decisions/00022-service-ipc-bitcode.md)); no existing command changes
  position. An older service sent it drops the connection, and the client reports "Lore service closed
  connection without sending a result" (`lore/src/remote/call.rs`). The handshake
  [ADR-00025](../developing/decisions/00025-service-version-compatibility.md) proposes would turn that into a
  clear refusal.
- **On-disk format** — N/A: a clone writes the same files and store keys as any instance. The in-progress
  directory is a sibling of the destination, not part of any instance. The in-progress mark, which a crash
  between the rename and its removal can leave in an instance's Lore directory, is a file older clients
  ignore.
- **CLI and public API** — New: `lore repository instance clone <destination> [--allow-copy]`, which prints
  the new instance's path and ID and exits 0, or names the refusal (no block sharing, unsupported provider,
  not enough space, and the existing destination and source checks) and exits non-zero as other instance
  commands do; `--dry-run` prints the provider's answer; `lore_repository_instance_clone` and its `_async`
  form, with an arguments struct holding the destination and the allow-copy flag; and the
  `RepositoryInstanceClone` events. An interrupted clone can leave `<destination name>.cloning-<new
  instance ID>` beside the destination, a name users see and may delete. Existing commands and structs are
  unchanged.
- **Filesystem provider interface** — `FilesystemProvider` gains the clone answer and the clone itself, both
  defaulting to *unsupported*, so existing providers compile and refuse clones until they implement them.

## Non-Functional Considerations

- **Concurrency** — The clone holds the source's repository lock, and with it the shared store's locks
  (ADR-00024), for the whole clone; see Lock hold for the times per backend. Every other process using the
  store waits that long for its next command. Before the locks are taken, the clone probes the pair, writing a
  probe file in the source's Lore directory and its clone in the destination's parent, checks the space, takes
  the in-progress directory's lock file beside the destination, and removes directories that earlier
  interrupted clones left there. Builds and editors in the source are not Lore commands and can still write
  during the clone; see Risks.
- **Memory** — Preserved: the clone walks the tree without holding it; the filesystem does the copying.
- **Threads** — The walk runs on the calling thread and on threads lent from the syscall pool's cap, at
  most half of it, so the clone never takes the threads every other file operation in the process runs on,
  and the pool's threads and the walk's stay within the pool's cap. Measured on APFS on an M3 Max, over
  100,000 files of 4 KiB in 11,000 directories, three runs each, one walk thread against the calling thread
  plus eight lent ones: a copy took 15.6 to 19.0 s against 6.9 to 7.0 s, and whole-directory `clonefile`
  1.15 to 1.24 s against 1.19 to 1.27 s, so lent threads speed up a copy and do not speed up `clonefile`. On
  APFS the clone first lists each directory of the source once, on the same threads, to find nested
  repositories: on the same tree that took 0.14 to 0.19 s, against 1.24 to 1.25 s for the whole-directory
  clone, and 0.33 to 0.37 s on one thread.
- **Statelessness** — Preserved: the backend choice is made per clone from the probe and not cached across
  operations.
- **Determinism** — Preserved: a clone's revision, branch and staged state equal its source's, whichever
  backend made it.
- **Disk** — On a block-cloning backend a clone costs its filesystem metadata: measured about 280 B per entry
  on APFS. NTFS keeps at least one entry per file and directory in its
  [master file table](https://learn.microsoft.com/en-us/windows/win32/fileio/master-file-table), of a fixed
  record size that
  [`FSCTL_GET_NTFS_VOLUME_DATA`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_get_ntfs_volume_data)
  reports, assumed here to be 1 KB, so a copy on NTFS adds on the order of 10 GB of metadata at ~11M
  entries on top of the content (estimate); ReFS's per-entry metadata cost is not measured.
  Each clone then grows by every byte it rewrites.

## Migration Plan

N/A — no breaking changes, no migration required.

## Security Considerations

No change to the trust model. A clone is a copy of a working tree the user already has, written with the
user's permissions. It copies symbolic links as links and never follows one, and leaves out a directory
replaced by a link after it was listed. Nor does a link swapped in above a directory while the clone lists it
lead the clone out of the tree. On Unix the clone reads each entry through its directory's open handle. On
Windows it reads each entry by path, and holds each directory it lists open without sharing delete access, so
a rename or removal of that directory, or of one above it, is refused until the directory is listed. So a
crafted tree cannot make the clone read or write outside the source and the destination, and the destination
checks run on the destination's resolved parent. The probe writes one small file in the source's Lore
directory and clones it into the destination's parent, with the same permissions, and removes both.

## Privacy Considerations

No new data leaves the machine. A clone registers its path in the shared store, as any instance does.

## Risks and Assumptions

**Assumptions**

- **Assumption:** the trees cloned are like a typical full sync of the large game project: about 1 TB in
  about 11 million entries, on Mac and Windows, Windows the majority — *invalidated if:* trees are much
  smaller, in which case even a copy is cheap and the refusal matters less; the design does not depend on it.
- **Assumption:** the users who need many working trees keep them on a filesystem that clones: APFS, the
  macOS default, Dev Drive or ReFS on Windows, or Btrfs or XFS on Linux — *invalidated if:* most such users
  keep trees on NTFS or ext4, where the clone refuses and a copy holds the store's locks for hours.
- **Assumption:** a Dev Drive clone holds the locks for tens of minutes at ~1 TB, as Lock hold estimates —
  *invalidated if:* a measurement on Windows 11 24H2 with Dev Drive shows hours, in which case even the
  block-cloning path needs a shorter lock hold.

**Risks**

- **Risk:** a build or editor writes in the source during the clone, so the clone captures a mix of before
  and after — *mitigation:* a tracked file that changed has a modification time its record does not match, so
  the clone's first scan hashes it and reports it. An untracked build output caught mid-write is not checked
  by Lore; accepted, and a clone made while the source builds may need that build output rebuilt.
- **Risk:** Microsoft Defender's real-time protection scans each file the clone creates on Windows, which can
  dominate a copy's time and adds to a Dev Drive clone's — *mitigation:* Lock hold's Windows estimates include
  it as an assumption to measure; Dev Drive's
  [performance mode](https://learn.microsoft.com/en-us/defender-endpoint/microsoft-defender-endpoint-antivirus-performance-mode)
  scans asynchronously, and the refusal message points copy users to Dev Drive.
- **Risk:** an interrupted clone leaves its in-progress directory on disk — *mitigation:* it carries its own ID
  and no registration, so it is not an instance; a later clone to the same destination removes it. A clone that
  fails, rather than crashes, names the directory in its error, removes it and deletes its anchor keys where
  the store allows. *Accepted:* after a crash, the clone's three small anchor keys stay in the store, and
  nothing removes them.
- **Risk:** the probe says a pair of volumes shares blocks, but some entries cannot be cloned (for example a
  nested subvolume or a mount point inside the tree) — *mitigation:* the clone stops and fails rather than
  copying those entries silently, unless `--allow-copy` was given.
- **Risk:** binaries a build made in the source point their debug information at the source's object
  files until relinked — *mitigation:* accepted; it affects build tools, not Lore.
- **Risk:** the clone waits for as long as other commands in its process, such as the service's, keep sharing
  the source's lock — *mitigation:* accepted; making new commands queue behind a waiting clone would deadlock
  a command that opens the same instance twice, and each such command finishes on its own.

## Drawbacks

- On a filesystem that cannot clone, the user must opt into a copy that costs a full tree and hours of lock
  hold.
- Other instances on the store wait for the whole clone: minutes on APFS, an estimated tens of minutes on Dev
  Drive.
- Each clone holds its own copy of the filesystem metadata, about 1.4 GB for the measured tree on APFS.
- Clones that sync forward or build stop sharing blocks, so they suit short-lived or slowly diverging work.

## Alternatives Considered

### Status quo: a second instance over the shared store

*Rejected because:* it writes every file its view selects, 456 GB to 1 TB per instance for a view a build
needs. This proposal does not give its time: the machine above had 30 GB free and could not hold a second
full copy.

### Fall back to a copy silently

An earlier draft of this proposal copied whenever the filesystem could not clone.

*Rejected because:* on NTFS or across volumes that turns a minutes-long command into one that writes up to
1 TB and holds every instance's store locks for hours, without warning. Refusing, with an opt-in that states
the cost, keeps that choice with the user.

### Choose the clone implementation by OS

Pick block cloning per target OS at build time and find out per file whether it cloned, as the prototype does.

*Rejected because:* the filesystem decides, not the OS. A Windows machine can hold NTFS and Dev Drive
volumes side by side, Linux has both ext4 and reflink filesystems, and no filesystem clones across volumes.
Knowing only per file, after writing, rules out refusing before the clone starts.

### Copy outside the store's locks, holding them only to register

*Rejected because:* a sync started in the source mid-clone would leave the copy holding files of two
revisions while its anchors name one, and its copied records would vouch for files they do not match. Holding
the locks throughout is the safe choice: the hold is minutes on APFS, and an estimated tens of minutes on Dev
Drive.

### Adopt a directory the user copied

A command that gives a directory the user copied with `cp -c` or `cp --reflink` a new identity, registers
it and copies the source's anchors.

*Rejected because:* Lore cannot hold the source's repository lock across a copy it does not run. A copy
taken during a sync holds files of revision A while the anchors it adopts name revision B. Its copied
records vouch for the revision-A files' times, so the first scan skips them and status reports the tree as
clean at revision B while it is not.

### Copy the source's records into the store for the clone

*Rejected because:* that is 1.1 million store writes under the exclusive store lock per clone of the measured
tree. [Instance-local modified-time records](2026-11-03-instance-local-modified-time-records.md) carry them
with the directory instead.

## Prior Art

- Git worktrees share the object store between working trees and keep each one's index and state in a
  per-worktree directory ([git-worktree](https://git-scm.com/docs/git-worktree)). A new worktree checks out
  every file, as a second Lore instance does. `git clone --local` hard-links objects from a repository on the
  same machine ([git-clone](https://git-scm.com/docs/git-clone)), the nearest precedent for cloning locally.
- GNU `cp --reflink` distinguishes `always`, which fails when the filesystem cannot clone, from `auto`,
  which falls back to a copy ([cp](https://man7.org/linux/man-pages/man1/cp.1.html)). This proposal's default
  is `always`; `--allow-copy` is `auto`.
- Jujutsu workspaces share one repository between working copies, each with its own working-copy commit
  ([jj workspaces](https://jj-vcs.github.io/jj/latest/working-copy/#workspaces)). Worth borrowing: each
  workspace's state belongs to it, as instance-local state does here.
- Sapling with EdenFS serves the working copy virtually, fetching file content on demand
  ([EdenFS](https://github.com/facebook/sapling/blob/main/eden/fs/docs/Overview.md)). The provider path
  above covers virtual instances the same way.
- Perforce lets one machine hold many client workspaces of a depot, each holding its own copy of the files
  it maps ([p4 client](https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_client.html)).
  Worth avoiding: the disk cost this proposal removes.

## Unresolved Questions

- Which measurements confirm the estimates? None is planned before acceptance; each would replace an
  estimate in Lock hold:
  - **macOS, APFS:** a clone of a full ~1 TB tree: wall time, `df` delta and first `status --scan` time.
  - **Windows, Dev Drive (Windows 11 24H2):** a clone of the measured tree and of a full tree, with Defender in
    its default mode and in performance mode: wall time, `df` delta, first scan time, and the per-file cost of
    duplicating extents.
  - **Windows, NTFS with `--allow-copy`:** a copy of the measured tree, with Defender on: wall time and
    metadata growth.
  - **Linux, Btrfs and XFS:** a clone of the measured tree: wall time and `df` delta.
  - **Any platform:** how fast a clone stops sharing as it syncs forward, as `df` growth per day of syncs.
- Can SWFS's mount manager mount a cloned external instance directory at a new path without restarting the
  service, as the Virtual instances sketch assumes?
