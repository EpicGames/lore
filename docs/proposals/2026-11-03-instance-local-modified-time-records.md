---
lep: 2026-11-03-instance-local-modified-time-records
title: Instance-local modified-time records
authors:
  - Zack Neyland
status: Draft
created: 2026-11-03
updated: 2026-11-09
discussion: https://web.lore.internal.epicgames.net/epic/Lore/change-request/1110
---

# Instance-local modified-time records

## Summary

Lore records, for each file in a working tree, the modification time at which the file last held its
revision's content, so a scan can skip hashing a file whose time has not moved. This proposal moves those
records out of the shared mutable store and into the instance's own Lore directory (`.lore/`). Records only
one instance reads stop growing the store every instance on it loads, and a copy of the instance's
directory carries records that already describe its files.
[ADR-00029](../developing/decisions/00029-instance-local-modified-time-records.md) records the decision.

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

## Goals / Non-Goals

### Goals

- An instance's new modified-time records add nothing to the shared store.
- A copy of an instance's directory starts with records that describe its files.
- Neither an upgraded client nor, after a downgrade, an older one trusts a record for content the path no
  longer holds, outside the cases Durability and Risks name.

### Non-Goals

- SWFS instances. Their mount presents the recorded times to applications (`lore-revision/src/fs/swfs/`
  reads them with `file_modified_time`), so their records stay in the store as today.
- In-memory instances. Their records would outlive the store, and so the revision, they describe, so they
  stay in the store as today.
- Giving a copy a new instance identity. The
  [copy-on-write instance clone](2026-10-31-copy-on-write-instance-clone.md) proposal does that.
- Restoring a file's committed modification time on sync, for build systems that want the original
  timestamp. Instance-local records do not preclude it.
- Moving anchors or the instance registry. Other instances read them, so they stay in the mutable store as
  [ADR-00014](../developing/decisions/00014-anchor-tracking-in-mutable-store.md) decided.

## Proposed Design

- **Records live in the instance.** An instance whose working tree is a directory keeps its records in
  `.lore/records/`. A record holds the same fact as today, that the path held the current revision's
  content at that modification time, under a key derived from the lowercased path and never the instance.
  (Goal 1.)
- **A copy inherits them.** Because the key does not name the instance, the records in a copied `.lore/`
  describe the copy's files, as long as the copy keeps modification times. A lookup that falls back to the
  store copies the record it finds into `.lore/`, so records an older client wrote reach a copy too once a
  scan has read them. (Goal 2.)
- **A new instance never touches the store's records.** An upgraded client that creates an instance, by
  `lore repository create` or `lore clone`, clears `.lore/records/` and marks it with an empty
  `.lore/records/no-store` file. The store holds no records under an instance ID that is new, so a marked
  instance's lookups never read the store and its new times zero nothing there. An older client run on it
  after a downgrade can write some; Risks covers them. A copy of `.lore/` carries the mark, and so would an
  instance made by the
  [copy-on-write instance clone](2026-10-31-copy-on-write-instance-clone.md), whose instance ID is new.
  (Goals 1 and 3.)
- **For an older instance, the store is a fallback that does not go stale.** An instance without the mark,
  created by an older client, may hold store records. Its lookups that find no record in `.lore/` read the
  record an older client wrote in the store. Copying such a record into `.lore/` leaves it in the store,
  since it still describes the path's content. When an upgraded client records a new time for a path, or
  clears it, it also zeroes that path's store record if one exists. A store record therefore either still
  describes the path's content or answers nothing, for this client and for an older one after a downgrade,
  apart from the crash case under Durability. An older instance is never marked, so it keeps the fallback
  for as long as it exists. The cost is a store lookup for each time recorded or cleared, and one store
  write per path whose content changes while an older client's record of it remains. (Goal 3.)
- **The records are sorted arrays.** The records are split into shards by the first byte of the key.
  Each shard is an array of records sorted by key, and its file is a header followed by the same bytes
  (Compatibility gives the layout). Loading a shard copies its file into memory without decoding, in one
  read for a tree of up to about 2.8 million files and in two above, and writing it back is one gathered
  write from its memory. A lookup is a binary search. A new time for a key the shard holds is changed in
  place; new keys are collected and merged into the array in one pass. A removed record is marked with
  time 0 and left out when the shard is written back.
- **Records are advisory.** A record that is missing or cannot be read costs one hash, never a wrong
  answer, as today.

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

  A client treats a shard file as absent, hashes, and replaces it at its next write-back when the header
  or version is not one it recognizes, when the body is not a whole number of records, or when it holds
  more than 2^22 records. An older client
  ignores the directory and reads only the store. After a downgrade it hashes the files whose records
  moved to `.lore/` and finds the rest in the store as before. It does not misread a file, since an
  upgraded client zeroes every store record it makes stale, apart from the crash case under Durability
  and the downgrade under Risks.
- **`.lore/` contents** — Backup and copy tools see the new directory, about 24 bytes per file of the
  working tree. Copying it with the working tree, keeping modification times, is what lets a copy skip
  hashing.
- **CLI and public API** — N/A: no command, output or API changes.

## Non-Functional Considerations

- **Concurrency** — Commands still take the store locks, and the repository lock inside them, as
  ADR-00024 orders. A new instance's records add nothing to the buckets they load and rewrite under the
  store lock, and it loads no bucket to look for records of its own. The entries older clients wrote,
  zeroed or not, stay in those buckets. Each shard has its own async read-write lock, which is never held
  by a task waiting for anything but that shard's file; lookups in one shard run side by side.
- **Memory** — The records grow with the number of files, as they do today: 24 bytes per record, in memory
  and in `.lore/records/`, against 84 bytes per record in the store: 26 MB for the 1,096,884 records of the
  working tree measured here, which the store would hold as about 92 MB. A command loads only the shards
  the paths it visits reach. New keys wait to be merged in a list of their own, which a shard merges once
  it holds more than an eighth of the shard's records or 256 entries, whichever is larger, and at the
  latest when the shard is written back.
- **Statelessness** — The loaded records belong to the instance's repository lock, which the process
  caches and shares between commands that hold it at once (`REPOSITORY_LOCK_CACHE` in
  `lore-revision/src/repository.rs`). In the Lore service they therefore live while any command in the
  instance runs. Lore writes the changed shards back, side by side and alongside the store flushes, in
  the flush that follows each command with write access, and keeps the repository lock until those
  writes have landed. A read-only command's changes, the times it adopts from the store and the shards
  it could not read, are written by the flush of a later command with write access that shares the
  lock, and otherwise are dropped with the lock; the next command reads them again.
- **Write-back latency** — The flush after each command with write access rewrites every shard the
  command changed, whole, and the command keeps the repository lock until the writes land. A command
  that changes a record in every shard rewrites all of `.lore/records/`, 26 MB for the tree measured
  here. The writes run alongside the store flushes, so they lengthen the flush only where they take
  longer than the store's. A read-only command writes nothing.
- **Determinism** — Preserved, apart from the cases Durability and Risks name: records change only which
  files a scan hashes. In those cases a store record can match a file whose content changed, and an older
  client's scan after a downgrade then reports the file unchanged.
- **Durability** — A crash before Lore writes the records back loses the records changed since the last
  command's flush. A lost record costs a hash. For an instance an older client created, the `.lore/`
  records and the store's zeroing are written separately, so a crash between them can leave an older
  client's store record for content the path no longer holds; it answers only if the file's time is set
  back to the recorded one, and an upgraded client never reads it once the path has a record of its own.

## Migration Plan

No migration step: records move to `.lore/` as an upgraded client reads or changes their paths. What each
client reads and writes depends on which client created the instance:

| Instance created by | Client running | Reads | Writes |
| --- | --- | --- | --- |
| An older client | Upgraded | `.lore/records/`, then the store for a path with no record | `.lore/records/`, and zeroes the path's store record |
| An upgraded client (marked) | Upgraded | `.lore/records/` | `.lore/records/` |
| Either | Older, after a downgrade | The store | The store |

Records an older client wrote stay readable in the store as the fallback, and stop answering as the paths
they describe change. Their 84-byte entries stay in the buckets, as zeroed keys do today: the store keeps
an entry whose value is set to zero (`MutableStoreBucket` in `lore-storage/src/local/mutable_store.rs`).

Rollback is a downgrade and needs no step. The older client hashes the files whose records moved to
`.lore/` and records them in the store again; the slower first scan is the only signal. It misreads a file
only in the cases Durability and Risks name. Upgrading again reads `.lore/records/` as it was left, which
the third risk covers.

## Security Considerations

No change to the trust model. The records are in the instance's own directory, with the user's
permissions, and state only modification times. A crafted or foreign `.lore/records/` can make a changed
file with a matching time look unchanged, but writing it takes the same access as editing the working tree
itself, and a store record can do the same today. A record file whose structure does not parse is treated
as absent.

## Privacy Considerations

Less data in shared state: an instance's new records, which carry hashed paths and times, stay in its own
directory. Records an older client wrote stay in the shared store, even after the instance is deleted, as
they do today: deleting an instance removes only its registration and anchor keys
(`remove_instance_keys` in `lore-revision/src/instance.rs`).

## Risks and Assumptions

**Assumptions**

- **Assumption:** no other instance needs another instance's modified-time records — *invalidated if:* a
  feature reads them across instances.

**Risks**

- **Risk:** an older client runs on an instance an upgraded client created, after a downgrade, and writes
  store records under its instance ID, which the upgraded client neither reads nor zeroes once upgraded
  again — *mitigation:* the upgraded client never trusts those records. Only a second downgrade reads
  them. Such a record answers wrongly only for a path whose file keeps the recorded time while the content
  the current revision holds for it changes under the upgraded client, as when it clears a record.

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
- **Risk:** records for paths the tree no longer holds accumulate in `.lore/records/` — *mitigation:*
  clearing a path removes its record; what remains is bounded by the paths the instance has held.

## Drawbacks

- Records exist in two places, `.lore/records/` and the store fallback, for as long as an instance still
  has records an older client wrote.
- An instance an older client created is never marked, so it pays a store lookup for each time it records
  or clears for as long as it exists, even once every path has a record in `.lore/`.

## Alternatives Considered

### Keep the records in the store and copy them for a new instance

*Rejected because:* a copy of the 1.1-million-file instance measured here would write 1.1 million records
under the exclusive store lock, and the store would still grow by about 92 MB per instance.

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
