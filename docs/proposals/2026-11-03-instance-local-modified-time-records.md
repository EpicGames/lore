---
lep: 2026-11-03-instance-local-modified-time-records
title: Instance-local modified-time records
authors:
  - Zack Neyland
status: Accepted
created: 2026-11-03
updated: 2026-11-17
discussion: https://web.lore.internal.epicgames.net/epic/Lore/change-request/1110
---

# Instance-local modified-time records

## Summary

Lore records, for each file in a working tree, the modification time at which the file last held its
revision's content, so a scan can skip hashing a file whose time has not moved. This proposal moves those
records out of the mutable store: into the instance's own Lore directory (`.lore/`), or, for an instance
opened with in-memory stores other than an SWFS instance, into memory beside them. Records only one instance
reads stop growing the store every instance on it loads, and a copy of the instance's directory carries
records that already describe its files. A FUSE virtual clone's mount reports the records' times as the
modification times of the files it serves from the store, so a copy of a virtual instance reports the times
its source reports.
[ADR-00037](../developing/decisions/00037-fuse-mount-reports-modified-time-records.md) records the decision,
superseding [ADR-00029](../developing/decisions/00029-instance-local-modified-time-records.md).

## Motivation

- **The records are per instance but live in shared state.** The mutable store keys each record by a hash
  of the salt, the instance and the lowercased path (`file_modified_time_key` in
  `lore-revision/src/state.rs`). No other instance reads them.
- **They grow the shared store with every file of every instance.** One record is a 16-byte partition, a
  32-byte key and a 32-byte value (`MutableStoreEntry`, `lore-storage/src/local/mutable_store.rs`) plus a
  4-byte index entry, 84 bytes in all, and Lore keeps one per file. The working tree of a large game
  repository measured here holds 1,096,884 records, one per file (its 3.9 million directories have none),
  so about 92 MB per instance. A command loads the store's buckets on use and a flush rewrites whole buckets (`MutableStoreBucket` in the same file; retention between uses is opt-in, per
  [ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)), so
  every instance on the store pays for every other instance's records.
- **A copy given an identity of its own cannot use them.** The records name the instance ID, so a directory
  copied and given a new instance ID has no records that apply to it, and its first `lore status --scan`
  hashes every file.
- **A virtual clone's times cannot travel with a copy either.** v0.14.0's FUSE mount reports, for each file a
  sync or switch changed, a time it stores in the mutable store under keys named by the instance
  (`RecordedTimes` in `lore-revision/src/fs/fuse/attr.rs`), and reads the store for each new inode. A copy
  given a new instance ID reports the base time for every such file, older than its content, and a build
  tool comparing times can skip a rebuild. A committed file reports the base time again once the kernel
  forgets its inode, and a file a reset serves from the store again reports the time it had before the edit
  the reset discards, older than the outputs built from the edit, so a build tool skips their rebuild.
- **An in-memory instance writes its records to its mutable store.** In v0.14.0, an instance opened with
  in-memory stores (the `in_memory` global argument, `LoreGlobalArgs` in `lore-revision/src/interface.rs`),
  which a process keeps across the commands it runs in it, keeps its times in its in-memory mutable store, the
  same local mutable store without a directory (`create_client_memory_stores` in
  `lore-revision/src/repository.rs`), under a key that names the instance, as an SWFS instance does (v0.14.0
  in [`docs/release-notes.md`](../release-notes.md): "SWFS and in-memory instances keep their times in the
  store"). A record takes 84 bytes there against 24 in `.lore/records/`, and each time recorded, looked up or
  cleared is a store access under a key hashed from the instance and the path, where an instance on disk
  searches a sorted shard in memory.

## Goals / Non-Goals

### Goals

- An instance's new modified-time records add nothing to the shared store, outside a case Risks names.
- A copy of an instance's directory starts with records that describe its files.
- Neither an upgraded client nor, after a downgrade, an older one trusts a record for content the path no
  longer holds, outside the cases Durability and Risks name.
- A copy of a virtual clone reports the times its source reports, a committed file in one keeps the time it
  showed, and a file a reset serves from the store again reports a time later than the files written before
  the reset.
- An in-memory instance's commands keep its records in the form `.lore/records/` holds them, in memory,
  for as long as its stores last, and write none to its mutable store.

### Non-Goals

- SWFS instances, in-memory ones included. Their mount reads the recorded times it presents to
  applications from the store (`lore-revision/src/fs/swfs/` reads them with `file_modified_time`), and its
  instances cannot be copied yet, so their records stay in the store as today.
- A FUSE mount over an in-memory instance. A virtual clone serves the files it holds from its stores, so
  over stores that end with the process, they end with it. Such a mount opens `.lore/records/` by its Lore
  directory, as over any other instance (`RecordedTimes` in `lore-revision/src/fs/fuse/attr.rs`). That set
  outlives the stores it describes, and does not see the times the instance's commands keep in memory.
- A `lore clone --no-tracking`. Its stores, and the times it records in its in-memory mutable store, end
  as the command finishes, which leaves no instance to use them.
- Giving a copy a new instance identity. The
  [copy-on-write instance clone](2026-10-31-copy-on-write-instance-clone.md) proposal does that.
- Restoring a file's committed modification time on sync, for build systems that want the original
  timestamp. Instance-local records do not preclude it.
- Moving anchors or the instance registry. Other instances read them, so they stay in the mutable store as
  [ADR-00014](../developing/decisions/00014-anchor-tracking-in-mutable-store.md) decided.

## Proposed Design

- **Records live in the instance.** An instance whose working tree is a directory and whose stores are on
  disk keeps its records in `.lore/records/`. A record holds the same fact as today, that the path held the
  current revision's content at that modification time, under a key derived from the lowercased path and
  never the instance. (Goal 1.)
- **A copy inherits them.** Because the key does not name the instance, the records in a copied `.lore/`
  describe the copy's files, as long as the copy keeps modification times. A lookup that falls back to the
  store copies the record it finds into `.lore/`, so records an older client wrote reach a copy too once a
  scan has read them. (Goal 2.)
- **A new instance never touches the store's records.** An upgraded client that creates an instance whose
  stores are on disk, by `lore repository create` or `lore clone`, clears `.lore/records/` and marks it with
  an empty `.lore/records/no-store` file. The store holds no records under an instance ID that is new, so a
  marked instance's lookups never read the store and its new times zero nothing there. An older client run on
  it after a downgrade reads and writes some, and so does a command that shares a repository lock as the
  in-memory bullet below describes; Risks covers both. A copy of `.lore/` carries the mark, and so would an
  instance made by the [copy-on-write instance clone](2026-10-31-copy-on-write-instance-clone.md), whose
  instance ID is new. (Goals 1 and 3.)
- **For an older instance, the store is a fallback that does not go stale.** An instance without the mark,
  created by an older client, may hold store records. Its lookups that find no record in `.lore/` read the
  record an older client wrote in the store. Copying such a record into `.lore/` leaves it in the store,
  since it still describes the path's content. When an upgraded client records a new time for a path, or
  clears it, it also zeroes that path's store record if one exists. A store record therefore either still
  describes the path's content or answers nothing, for this client and for an older one after a downgrade,
  apart from the crash case under Durability. An older instance is never marked, so it keeps the fallback
  for as long as it exists. The cost is a store lookup for each time recorded or cleared, and one store
  write per path whose content changes while an older client's record of it remains. (Goal 3.)
- **An in-memory instance keeps them in memory.** An instance opened with in-memory stores keeps its
  records in memory, in shards of the same form, and every command over those stores shares them, whatever
  repository lock it holds. No command over the instance's stores on disk, and no mount, reads or writes
  them, and no command over the in-memory stores reads `.lore/records/` or writes back any of its shards,
  since its flush does not reach the stores on disk those shards describe. The records start empty and
  never read the store, as a new instance's records do. Lore never writes them to disk, and they last until
  the process releases the stores, so they never outlive the revision they describe. The instance's mutable
  store takes no record. (Goal 5.) A command over the instance's stores on disk that shares the repository
  lock a command over its in-memory stores took keeps its times in the mutable store on disk, as Risks
  describes.
- **The records are sorted arrays.** The records are split into shards by the first byte of the key.
  Each shard is an array of records sorted by key, and its file is a header followed by the same bytes
  (Compatibility gives the layout). Loading a shard copies its file into memory without decoding, in one
  read for a tree of up to about 2.8 million files and in two above, and writing it back is one gathered
  write from its memory. A lookup is a binary search, then, for a key the array does not hold, a scan of the
  new keys not merged yet. A new time for a key the shard holds is changed in
  place; new keys are collected and merged into the array in one pass. A removed record is marked with
  time 0 and left out when the shard is written back.
- **Records are advisory.** A record that is missing or cannot be read costs one hash, never a wrong
  answer, as today. A virtual clone is the exception the next point describes.
- **A virtual clone's mount reports them.** A FUSE virtual clone's mount
  ([`lore-fuse`](../../lore-fuse/README.md)) reports, as the modification time of a file or directory it
  serves from the store, the time the instance's records hold for the path, and the instance's base time, kept
  in its configuration and set when the first instance of its line was created, where they hold none. A sync
  or switch writes no file in a virtual clone, so no file takes a new time from the filesystem: the *swap*
  that puts the revision in place in the mount records its time for each path it changes, and writes the
  records back before it publishes, synced under `--sync-data`. A commit records the modification time it read
  for each file it stores, however recent, so a committed file keeps the time it showed, and writes the records
  back, synced under `--sync-data`, before it serves those files from the store. A write that serves a file
  from the store again, as a reset of an edited file to the content the mount serves does, records one time
  per operation, read from the filesystem's clock as the operation first serves such a file, as a swap reads
  its time. The file then reports a time later than the files written before the reset, the outputs built
  from the edit it discards among them, apart from any written in the clock's last tick before the read. The
  operation sets the time in the records before it serves each file, and writes the records back, synced
  under `--sync-data`, as it ends. The mount and the instance's
  commands, which all run in the Lore service, share one loaded set of records. The provider tells a changed
  file by address and content and reads no record to do so, so in a virtual clone a record is the time the
  mount reports, not an observation a scan verifies. A missing record makes the mount report the base time,
  older than the content, and a build tool that compares times can then skip a rebuild. A copy of `.lore/`,
  as the [copy-on-write instance clone](2026-10-31-copy-on-write-instance-clone.md) makes, carries the times.

## Compatibility

- **Wire format** — N/A: no message or fragment changes.
- **Client/server protocols** — N/A: the server and the client-to-service IPC are unchanged.
- **On-disk format** — New directory `.lore/records/`, holding:
  - 256 shard files, named by the shard's index as two lowercase hex digits (`00` to `ff`). Shard `n`
    holds the records whose key starts with byte `n`.
  - In each, an 8-byte header, the magic `LOMT` and the format version (1) as a little-endian `u32`,
    then the records, sorted by key in ascending unsigned byte order, with no key twice.
  - Records of 24 bytes: a 16-byte key, the first 16 bytes of a hash of the repository's salt and the
    lowercased path, then the modification time as a little-endian `u64`. Little-endian makes a shard
    read the same on every host. A removed record, time 0, is never written.
  - The `no-store` mark, an empty file.

  A client treats a shard file as absent, hashes, and replaces it at its next write-back when the header or
  version is not one it recognizes, when the body is not a whole number of records, or when it holds more than
  2^22 records. An older client ignores the directory and reads only the store. After a downgrade it hashes
  the files whose records moved to `.lore/` and finds the rest in the store as before. It does not misread a
  file, since an upgraded client zeroes every store record it makes stale, apart from the crash case under
  Durability and the cases Risks names. In a virtual clone, a shard file treated as absent makes the mount
  report the base time for the paths it held, until a swap, a reset or a commit records them again, so a later
  format version costs a virtual clone a clean build after a downgrade.
- **FUSE swap times** — v0.14.0's FUSE mount also stored each swap's times in the mutable store, under
  keys taken from a hash of the salt, `fuse-recorded-time`, the instance ID and the lowercased path, beside
  a `fuse-recorded-times` marker, and read them for each new inode. An upgraded client neither reads nor
  writes those keys, and nothing zeroes them. v0.14.0, the only release with virtual clones (its section of
  [`docs/release-notes.md`](../release-notes.md)), also recorded every swap's times in `.lore/records/`
  (`swap_through` in `lore-revision/src/fs/fuse/swap.rs` called `record_modified_time` for each path it
  recorded), where an upgraded client finds them, apart from the times of a sync or switch that failed after
  its swap published. After a downgrade, v0.14.0 reads only its store keys, so it reports an older time, or
  the base time, for each path an upgraded client's swap changed or reset served from the store again; see
  Migration Plan.
- **In-memory instances** — N/A: an in-memory instance's stores, and the records its commands keep beside
  them, end with the process, so no record carries over between clients.
- **`.lore/` contents** — Backup and copy tools see the new directory, about 24 bytes per file of the
  working tree. Copying it with the working tree, keeping modification times, is what lets a copy skip
  hashing.
- **CLI and public API** — N/A: no command, output or API changes.

## Non-Functional Considerations

- **Concurrency** — Commands still take the store locks, and the repository lock inside them, as
  ADR-00024 orders. A new instance's records add nothing to the buckets they load and rewrite under the
  store lock, and it loads no bucket to look for records of its own, outside a case Risks names. The
  entries older clients wrote, zeroed or not, stay in those buckets. Each shard has its own async
  read-write lock, which is never held by a task waiting for anything but that shard's file; lookups in
  one shard run side by side.
- **Memory** — The records grow with the number of files, as they do today: 24 bytes per record, in memory and
  in `.lore/records/`, against 84 bytes per record in the store: 26 MB for the 1,096,884 records of the
  working tree measured here, which the store would hold as about 92 MB. An in-memory instance's records take
  the same 24 bytes, in memory alone. A command loads only the shards the paths it visits reach. New keys wait
  to be merged in a list of their own, which a shard merges once it holds more than an eighth of the shard's
  records or 256 entries, whichever is larger, and at the latest when the shard is written back. An in-memory
  instance writes no shard back, so its new keys wait until that threshold or a batch of times a command
  records merges them, and a record it removes stays until its shard next merges.
- **Statelessness** — The loaded records of an instance whose stores are on disk belong to the instance's
  repository lock, which the process caches and shares between commands that hold it at once
  (`REPOSITORY_LOCK_CACHE` in `lore-revision/src/repository.rs`). In the Lore service they therefore live
  while any command in the instance runs. Lore writes the changed shards back, side by side and after the
  store flushes, in the flush that follows each command with write access, and keeps the repository lock until
  those writes have landed; in a virtual clone, a swap, a commit and a reset that serves files from the store
  again also write them back during the command. A read-only command's changes, the times it adopts from the
  store and the shards it could not read, are written by the flush of a later command over the stores on disk
  with write access that shares the lock, and otherwise are dropped with the lock; the next command reads them
  again. A virtual clone's mount holds its set for as long as it serves, so its changes stay loaded between
  commands, and the instance's commands share that set. An in-memory instance's records instead belong to its
  in-memory stores, whatever repository lock its commands share: the process keeps them for the instance until
  it releases the stores (`repository_release` in `lore-revision/src/repository.rs`), and they end with the
  stores.
- **Write-back latency** — The flush after each command with write access rewrites every shard the
  command changed, whole, after the store flushes, synced to disk under `--sync-data` as the stores are,
  and the command keeps the repository lock until the writes land. A command that changes a record in
  every shard rewrites all of `.lore/records/`, 26 MB for the tree measured here, which adds to the
  flush's duration. In a virtual clone, a swap and a commit write every changed shard back before they
  serve the files they change, and wait for the write. A reset that serves files from the store again
  writes its shards back as its operation ends, and waits for the write. A read-only command writes nothing,
  and neither does a command over an in-memory instance's stores, even for a repository lock it shares.
- **Determinism** — Preserved, apart from the cases Durability and Risks name: in an instance on disk, records
  change only which files a scan hashes. In a virtual clone they decide the time the mount reports for a file
  it serves from the store, and change nothing a scan finds. In those cases a store record can match a file
  whose content changed, and an older client's scan after a downgrade then reports the file unchanged, as can
  a command the fourth risk describes.
- **Durability** — A crash before Lore writes the records back loses the records changed since the last
  write-back. A lost record costs a hash. For an instance an older client created, the `.lore/`
  records and the store's zeroing are written separately, so a crash between them can leave an older
  client's store record for content the path no longer holds; it answers only if the file's time is set
  back to the recorded one, and an upgraded client never reads it once the path has a record of its own.
  In a virtual clone a lost record makes the mount report an older time. Lore writes a swap's times back
  before the swap publishes, and a commit's after the commit's store flush and before it serves its files
  from the store, each synced under `--sync-data`. A commit whose records cannot be written back serves none
  of its files from the store, as one whose store flush fails does. A reset that serves files from the store
  again sets their time in memory as it serves each, and writes the times back once per operation rather
  than once per file, synced under `--sync-data`, as the operation ends. A crash between the two, or a
  write-back that fails, which the reset logs, and that the command's flush does not repeat, leaves the files
  served from the store with their earlier time once the mount restarts, so a build tool can skip a rebuild.
- **Lookup latency** — A virtual clone's lookup of a new inode reads its time from a shard in memory. The
  first lookup in a shard loads its file, up to 256 KB in one read, where v0.14.0 loaded a store bucket.

## Migration Plan

No migration step: records move to `.lore/` as an upgraded client reads or changes their paths. What each
client reads and writes depends on which client created the instance:

| Instance created by | Client running | Reads | Writes |
| --- | --- | --- | --- |
| An older client | Upgraded | `.lore/records/`, then the store for a path with no record | `.lore/records/`, and zeroes the path's store record |
| An upgraded client (marked) | Upgraded | `.lore/records/` | `.lore/records/` |
| Either | Older, after a downgrade | The store | The store |
| An upgraded client or v0.14.0, virtual clone | Upgraded | `.lore/records/`, for the times its mount reports only | `.lore/records/`, from swaps, resets and commits |
| An upgraded client or v0.14.0, virtual clone | v0.14.0, after a downgrade | `.lore/records/` for scans, and its store keys for the times its mount reports | `.lore/records/`, and its swaps the store keys |

v0.14.0 already keeps records in `.lore/records/`, so it is not the older client of the third row; only
its FUSE mount differs. A command over an instance's stores on disk that shares a repository lock a command
over its in-memory stores took reads and writes the store, whichever client created the instance; Risks
covers it.

Records an older client wrote stay readable in the store as the fallback, and stop answering as the paths
they describe change. Their 84-byte entries stay in the buckets, as zeroed keys do today: the store keeps
an entry whose value is set to zero (`MutableStoreBucket` in `lore-storage/src/local/mutable_store.rs`).

Rollback is a downgrade and needs no step for an instance on disk. The older client hashes the files whose
records moved to `.lore/` and records them in the store again; the slower first scan is the only signal. It
misreads a file only in the cases Durability and Risks name. Upgrading again reads `.lore/records/` as it
was left, which the third risk covers.

A virtual clone needs a clean build after a downgrade to v0.14.0: until a sync changes a path again, v0.14.0
reports a time older than its content for each path an upgraded client's swap changed or reset served from the
store again, and a build tool that compares times can skip a rebuild such a path needs, and nothing signals
it. The release note says so. An instance an upgraded client cloned from a virtual clone reports the base
time for every path under v0.14.0, which finds no store keys under its ID; the [copy-on-write instance
clone](2026-10-31-copy-on-write-instance-clone.md) proposal covers it. Upgrading again reads `.lore/records/`,
where v0.14.0's swaps recorded their times too.

## Security Considerations

No change to the trust model. The records are in the instance's own directory, with the user's permissions,
or, for an in-memory instance, in the memory of the process that serves it, and state only modification times.
A crafted or foreign `.lore/records/` can make a changed file with a matching time look unchanged, but writing
it takes the same access as editing the working tree itself, and a store record can do the same today. A
record file whose structure does not parse is treated as absent. In a virtual clone, the same access sets the
times its mount reports.

## Privacy Considerations

Less data in shared state: an instance's new records, which carry hashed paths and times, stay in its own
directory, or in the memory of the process that serves an in-memory instance, outside a case Risks names.
Records an older client wrote stay in the shared store, even after the instance is deleted, as they do today:
deleting an instance removes only its registration and anchor keys (`remove_instance_keys` in
`lore-revision/src/instance.rs`).

## Risks and Assumptions

**Assumptions**

- **Assumption:** no other instance needs another instance's modified-time records — *invalidated if:* a
  feature reads them across instances.

**Risks**

- **Risk:** an older client runs on an instance an upgraded client created, after a downgrade, and writes
  store records under its instance ID, which the upgraded client neither reads nor zeroes once upgraded again
  — *mitigation:* the upgraded client never trusts those records outside the fourth risk's case. Only a second
  downgrade, or a command that risk describes, reads them. Such a record answers wrongly only for a path whose
  file keeps the recorded time while the content the current revision holds for it changes under the upgraded
  client, as when it clears a record.

- **Risk:** a copy that does not keep modification times inherits records that never match —
  *mitigation:* a record that does not match costs a hash, so the copy scans as a fresh instance does today.
- **Risk:** an older and an upgraded client alternate on one instance. The older client records and
  clears times only in the store, and its clear, when a sync deletes a file, cannot reach
  `.lore/records/`, so after upgrading again the instance's own record for a path can describe content the
  older client replaced or deleted — *mitigation:* the own record still wins safely. It answers only while
  the file keeps the recorded time, and the older client changes a path's content only by writing or
  deleting its file, which leaves the path with no file or one of a new time, a recreated file
  included. The stale record then costs a hash. It answers wrongly only if a tool sets the file's time
  back to the recorded one, as a store record would today. For a path with no record of its own, on an
  instance an older client created, the store record describes what the older client saw, the guarantee
  records have today.
- **Risk:** a command over an instance's stores on disk shares the repository lock a command over its
  in-memory stores took in the same process, and so records, looks up and clears its times only in the store,
  as an older client does. It finds none of `.lore/records/`'s times, so it hashes each file it scans that has
  no store record, and writes an 84-byte store record for it, which no command that keeps its times in
  `.lore/records/` zeroes — *accepted:* it takes both kinds of command running on one instance in one process
  at once, and the commands that follow once that lock is released keep their times in `.lore/records/` again.
  A store record it writes answers wrongly only where the first risk's does, and a time it clears stays in
  `.lore/records/` as the third risk's does.
- **Risk:** records for paths the tree no longer holds accumulate in `.lore/records/` — *mitigation:*
  clearing a path removes its record; what remains is bounded by the paths the instance has held.
- **Risk:** a virtual clone downgraded to v0.14.0 reports times older than the content of the paths an
  upgraded client's swaps changed or resets served from the store again, and a build skips a rebuild —
  *mitigation:* the release note asks for a clean build after a downgrade.
- **Risk:** a sync or switch that v0.14.0 ran on a virtual clone failed after its swap published, so its
  times reached only v0.14.0's store keys, and an upgraded client reports the path's older record —
  *accepted:* it takes a failed sync before the upgrade, and the next sync that changes the path records it
  again.

## Drawbacks

- Records exist in two places, `.lore/records/` and the store fallback, for as long as an instance still
  has records an older client wrote.
- An instance an older client created is never marked, so it pays a store lookup for each time it records
  or clears for as long as it exists, even once every path has a record in `.lore/`.

## Alternatives Considered

### Keep the records in the store and copy them for a new instance

*Rejected because:* a copy of the 1.1-million-file instance measured here would write 1.1 million records
under the exclusive store lock, and the store would still grow by about 92 MB per instance.

### Keep a virtual clone's swap times apart from its records

A virtual clone could keep the times its mount reports in a directory of their own, in the same format.
*Rejected because:* a swap would then write the same time for the same paths twice, once for the mount and
once for the records, and a virtual clone reads its records for nothing else, so a second directory would
add a format's worth of loading, sharing and write-back for no gain.

### Write a virtual clone's swap times to the store as well

Writing each swap time to the mutable store too, under v0.14.0's keys, would keep a downgrade correct for the
paths a swap changed, though not for a commit's or a reset's times or for an instance an upgraded client
cloned.
*Rejected because:* each swap would cost one store write per path it changes, and the store would grow with
every path every virtual clone swaps, which keeping the records in the instance avoids. A downgrade's cost, a
clean build, is accepted instead.

### Keep an in-memory instance's records in its mutable store

*Rejected because:* each record costs 84 bytes there against 24, and each time recorded, looked up or
cleared is a store access where the records kept in memory search a shard.

### Keep an in-memory instance's records in `.lore/records/`

*Rejected because:* the records would outlive the in-memory stores, and so the revision, they describe.
Clearing them as a process first builds the stores does not help: two processes can each open the instance
with stores of their own and run commands in turn under its repository lock, each trusting the other's
times for content its own stores do not hold. Every command with write access would also pay a write-back
that no later process can use.

### Move every record out of the store on upgrade

The first upgraded command moves the instance's records into `.lore/` and zeroes them in the store.

*Rejected because:* for the instance measured here that is 1.1 million store reads and writes under the store
lock in one command, whatever that command was meant to do. Moving each record as its path is read
or changed spreads the same work across the commands that touch it.

### Keep the records in a per-instance store file

Give each instance its own mutable store in `.lore/` for its records.

*Rejected because:* the mutable store's entry costs 84 bytes per record against 24, and it carries buckets
and a partition the records do not need.

## Prior Art

- Git keeps the equivalent per working tree: each worktree's index under `$GIT_DIR/worktrees/<id>/` caches
  file times ([git-worktree](https://git-scm.com/docs/git-worktree)). Git's index also records inode and
  device numbers ([index-format](https://git-scm.com/docs/index-format)), so a copied tree forces a
  refresh; keying records on the path and time alone is what lets a copy here skip hashing.
- Mercurial keeps the same cache per working directory, in `.hg/dirstate`
  ([DirState](https://wiki.mercurial-scm.org/DirState)).

## Unresolved Questions

- Should deleting or pruning an instance also remove its records from the store, which no client does
  today?
