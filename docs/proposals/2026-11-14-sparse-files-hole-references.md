---
lep: 2026-11-14-sparse-files-hole-references
title: Hole references for sparse files in fragment lists
authors:
  - Mattias Jansson
status: Accepted
created: 2026-11-14
updated: 2026-11-14
discussion: https://web.lore.internal.epicgames.net/Epic/Lore/change-request/1300
---

# Hole references for sparse files in fragment lists

## Summary

A sparse file holds data in some ranges and holes, ranges that hold nothing, in the rest. Lore
stores a file as a fragment list: a list of references, each naming a fragment by hash and giving
the offset where it starts. Today a put reads a sparse file whole, so its holes are read, hashed and
stored as zeros, and a get writes them back as zeros, which allocates them. This proposal lets a
fragment list record a hole as a reference with the zero hash, which names no fragment. A new
`skip_holes` field on the `lore_storage_put_file` and `lore_storage_put_file_resolved` items asks a
put to read only the ranges the file system holds data for. A get into a file leaves each hole
unwritten, so the file it writes has the same holes. This is the initial implementation, for Linux
file systems that report holes.

## Motivation

Workspace disk images, file-system images such as XFS images, are large files that are mostly
holes. Storing them through the storage API costs what the image's size costs, not what its data
costs. Measured on Linux, on XFS, through the storage API's put and get paths:

- A 100 GiB XFS image holding 8 GiB of files, in 17 ranges of data. A put reads and hashes all
  100 GiB, in 75.6 s, and its fragment list holds 409,600 references (16.4 MB). A get writes all
  100 GiB, in 282 s, and the written file has all 100 GiB allocated.
- A generated 8 GiB image holding 2.1 GiB of random data in 235 ranges. A put reads and hashes all
  8 GiB, and a get writes all 8 GiB and allocates it.

Every get of such an image also needs free space for the whole image on the target, however
little of it is data.

## Goals / Non-Goals

### Goals

- A put of a sparse file reads, hashes, stores and uploads only its data, so its cost follows the
  data, not the file's size.
- A get into a file recreates the holes, so the file it writes is allocated like the source.
- The fragment list of a sparse file grows with the number of ranges of data, not with the file's
  size.
- Content stored without the new field, and every existing reader of it, is unchanged.
- No reader built before this change returns wrong content for content stored with holes.

### Non-Goals

- Turning zeros into holes. Zeros written to a file are data, and stay data.
- Holes in files the revision layer stores. A revision's files are stored without holes. Revision
  trees do not support content stored with holes, and nothing refuses it when a file is added to a
  tree or modified: if such an address is added anyway, FUSE reads of the file fail, and status
  reports the file modified whenever a scan hashes it.

## Proposed Design

### Hole references

A reference with the zero hash in a fragment list is a hole. It names no fragment, and stands for
the range from its offset to the next reference's offset, or to the end of the list's content for
the last reference. A hole reads as zeros. No store holds anything for it.

A list that holds a hole is valid only if:

- it holds at least two references, or one that is a hole (a file that is one hole);
- no hole follows a hole;
- every reference that is not a hole spans at most one fragment's worth of content, so a list that
  holds holes is a list of leaves, never of sublists;
- the offsets rise strictly, as in any list.

The server checks these rules on every list it is given. A list long enough to need sublists is
split as any list is, and a hole stays in the sublist of leaves that holds it.

### What a hole is

A hole is a range the file system holds no written data for: a range never written, or an extent
allocated and never written (an *unwritten* extent, such as one `fallocate` reserves). Zeros
written to a file are data. A file with a hole and the same file with zeros written over it are
different content, with different addresses. A file with no hole gets the same address with or
without `skip_holes`, and a file larger than one fragment that has holes gets a different one.

On Linux the put finds the ranges of data in the file's extent map, from the `FIEMAP` ioctl with the
flag that syncs the file first. Written extents are data; unwritten extents and gaps between extents
are holes. What the page cache holds does not change the result, so the same file always gets the
same address. Where a file system has no extent map, as tmpfs, the put falls back to `lseek` with
`SEEK_DATA` and `SEEK_HOLE`, which takes an unwritten extent as data wherever the page cache holds
pages for it. On its first `SEEK_DATA` lookup the put tells the system that it reads the source at
random, so it reads nothing ahead into a hole and leaves no pages there.

### Put

With `skip_holes` set, a put reads each range of data and chunks it as the item asks, and records
each hole between the ranges, and one at either end, as a hole reference. A hole is never read,
hashed, stored or uploaded. Fixed-size chunks keep the grid that starts at the beginning of the
file, and are cut short where a range of data starts or ends between two points of the grid, so a
chunk inside the data has the address it has without `skip_holes`. Content-defined chunking cuts
each range of data as a file of its own.

The field is a byte in what was padding after `remote_query` in both put items, so an item built
against a header without it has the same size and layout, and reads as zero for a caller that
zeroes the item, as `lore.h` asks since `remote_query`.

### Get

A get into a file empties the file it writes, then sizes it, before it writes anything, so every
range starts unallocated. The file is the `<path>.loretmp` the get stages through, or the target
itself for a direct write, which replaces the whole file. A hole is left unwritten, so it is a hole
of the written file on a file system that supports holes, and holds none of what that file held
before. Every range of data is written, zeros included. Holes in the source come back as holes, and
written zeros come back as written data. A put without `skip_holes` stores holes as data, which a
get writes back as data. A path takes one get at a time: items that name the same path, in one call
or in concurrent calls, are not supported.

Every other reader sees zeros for a hole: a read into memory or into a caller's buffer, and a
streamed read, which delivers a hole as pieces of zeros no larger than a fragment. A read with
fragment delivery (`fragments` on `lore_storage_get_resolved`) sends a hole as a `GET_FRAGMENT`
whose fragment has a `size_payload` of zero, which no stored fragment has, and no payload.

### Other walks of a list

Every walk that follows a list's references to fragments passes over a hole: obliterate and its
impact report, and the walks that collect the fragments a list references to query or push them.
A put's remote query never asks about a hole, as a hole uploads nothing. The FUSE fragment cache
refuses a list holding a hole: revision trees do not support content stored with holes, so FUSE
reads of a file whose address names such content fail.

## Compatibility

The results below are measured: content put with holes by this change, on Linux, read by a build of
`main` without it, through every read path of the C API and the CLI.

- **Wire format** — Unchanged. A list with holes is a list as before, with the zero hash in some
  references, and every peer decodes it.
- **Client/server protocols** — No new RPC or message. A client built before this change refuses
  content stored with holes with an error on every read that reaches a list holding a hole, from any
  server: `lore_storage_get_file`, `lore_storage_get` into a buffer, as events and streamed, with
  and without a range, all four `lore_storage_get_resolved` modes, and `lore file write --address`
  all fail with "fragment list entry N at content offset X has a zero hash". `lore_storage_get_file`
  writes no file. A ranged read that reaches only sublists without holes succeeds, as those
  sublists are lists as before. No path returns wrong or mis-sized content. A server built before
  this change accepts a push of content with holes, stores it and serves it, and a client with this
  change reads it back byte for byte. Content that is one hole, a file larger than one fragment that
  holds no data, is the exception: its list is the one reference of that hole, and a server built
  before this change refuses it with `InvalidFragment`, because its check of a list requires two
  references. A put of such content with `remote_write` to that server still completes: the item
  reports `stored_local` set and `stored_remote` clear, with no error, and the content stays local
  only, so a caller that needs it on the server checks `stored_remote`. Content stored without holes
  reads as before on both, through every path.
- **On-disk format** — Unchanged format. A store written by this change holds lists with hole
  references, which a build without it refuses to read, as above. No fragment flag changes.
- **CLI and public API** — `lore-capi` gains `skip_holes` in `lore_storage_put_file_item_t` and
  `lore_storage_put_file_resolved_item_t`, in what was padding, so the item sizes and every other
  field's offset stay as they are. `lore_storage_get_file` and `lore_storage_get_file_resolved`
  leave holes unwritten, and empty a `<path>.loretmp` an earlier get left before writing it. A hole
  in fragment delivery is a `GET_FRAGMENT` with no payload. No CLI command changes.
- **File systems** — Measured on Linux on four file systems: ext4, XFS and btrfs, where the put
  finds holes through `FIEMAP`, and tmpfs, which has no extent map, where it finds them through
  `SEEK_DATA` and `SEEK_HOLE`. On each, an image with holes put with `skip_holes` and got back read
  as the image, byte for byte, with only its data allocated; a file with zeros written into a
  range came back with that range allocated, as data; and a range `fallocate` reserved and never
  wrote was stored as a hole and came back unallocated.

## Non-Functional Considerations

- **Concurrency** — Preserved. A put reads one range of data at a time, as it reads a file today. A
  get writes the leaves of the ranges of data concurrently, as today, and writes nothing for a
  hole.
- **Memory** — Preserved. A hole costs one reference in a list and nothing in memory. A put keeps
  one window of a range in memory, as it keeps one of a file, and a streamed read delivers a hole in
  pieces no larger than a fragment, from memory shared by every read.
- **Statelessness** — Preserved. The change keeps no state across operations.
- **Determinism** — The address of a file put with `skip_holes` depends on how the file is
  allocated as well as on what it reads as, by design. On Linux, on a file system with an extent
  map, the same allocation always gives the same address, whatever the page cache holds. Content
  put without the field is unchanged.
- **Throughput** — Measured on XFS, on a shared machine whose disk was busy at times:
  - The generated 8 GiB image, with 2.1 GiB of data in 235 ranges. With `skip_holes` a put reads
    and hashes 2.1 GiB into lists of 364 KB (8,853 leaves and 236 holes); without, 8 GiB into lists
    of 1.31 MB (32,768 leaves). Without contention the put took 3.7 to 3.9 s and 6.4 to 7.0 s of
    CPU, against 3.6 s and 10.4 s, both bounded by writing the 2.1 GiB of new fragments to the
    store. A get took 0.6 to 1.0 s and allocated 2.1 GiB, against 4.2 s and 8 GiB.
  - The 100 GiB XFS image. With `skip_holes` a put reads 8 GiB into 32,797 references, against
    100 GiB into 409,600, and a get allocates 8 GiB, against 100 GiB.

## Migration Plan

No existing content changes, and `skip_holes` is off unless a caller sets it. Content stored with
holes is readable only by builds with this change, so a deployment upgrades every client that reads
such content before any caller sets `skip_holes`. Servers need no upgrade to store and serve content
stored with holes, except content that is one hole, which needs a server built from this change: a
server built before it refuses that list with `InvalidFragment`, because its check of a list
requires two references, and a put with `remote_write` to it completes with `stored_remote` clear
and no error, leaving the content local only. A rollback stops setting the field: content already
stored with holes stays readable by the upgraded clients, and the same file put again without the
field gets the address it has without holes.

## Security Considerations

The trust model is unchanged. A crafted list can claim a hole of any size within the content size
its root fragment states, as a crafted list can already claim content of that size; a get sizes the
file it writes to the content size as before, and a hole only leaves part of it unwritten. The
server checks the rules above on every list it is given, and every walk treats a list that holds a
hole as a list of leaves, refusing a reference in it that spans more than a fragment or names a
list. A hole names no fragment, so it reaches no stored data. No new data is integrity- or
confidentiality-sensitive.

## Privacy Considerations

A list stored with holes shows which ranges of the file hold data and which are holes. The data
ranges were already readable by anyone who can read the content; the difference between a hole and
written zeros is new, and describes how the file was written. No new identifiers, paths or metadata
reach a server, peer or log, and deleting or obliterating content works as before.

## Risks and Assumptions

**Assumptions**

- **Assumption:** the file systems sparse images live on report holes through an extent map —
  *invalidated if:* the images to be stored live on a file system without `FIEMAP`, where the
  `SEEK_DATA` fallback can take a cached unwritten extent for data.
- **Assumption:** a file is not written while it is put — *invalidated if:* writers change the
  file's allocation during a put, which changes the ranges the put reads, as writes during any put
  change what it reads. A file that shrinks while it is put with its holes skipped fails the put
  where the lost tail held data the put had already mapped, as the read meets the end of the
  file; a lost tail that held no data, or one walked with `SEEK_DATA`, is stored as a hole to the
  size at open.

**Risks**

- **Risk:** a file system that stores written zeros as holes, as some do when they compress, reports
  them as holes, and the put records holes there — *mitigation:* accepted; the file system has
  already turned those zeros into holes, and the get writes them back as holes.
- **Risk:** a client built before this change meets content stored with holes — *mitigation:* it
  refuses the content with an error on every path that reaches a hole, as measured above, and the
  Migration Plan upgrades readers first.

## Drawbacks

- The same bytes can have two addresses, one with holes and one without, so a file stored both ways
  is stored as two lists. Fixed-size leaves inside its data have the same addresses either way, and
  still deduplicate.
- Content stored with holes is unreadable by every client built before this change.

## Alternatives Considered

### A size field or flag on each reference

A reference could carry its length, or a flag marking it a hole, so a hole would need no implicit
length and no sentinel hash.

*Rejected because:* it changes the size of every reference, so every list ever stored would need a
second format beside it, and every reader would need both. The zero hash already names no content,
and a hole's length is already implied by the next reference's offset.

### A dedicated hole fragment

A put could store one fragment that stands for a hole, and reference it wherever a hole is.

*Rejected because:* a fragment holds its content, so a hole fragment would need a length of its own
for each size of hole, or a list could not tell how far it reaches. Every reader would load it as
content and write it as zeros, which allocates the hole on the target.

### Deduplicating zero leaves

A put could keep reading holes as zeros, which deduplicate to one stored leaf per size.

*Rejected because:* the put still reads and hashes the whole file, the list still holds a reference
for every leaf of the file, and a get still writes and allocates every zero. Zeros are also data:
a get cannot tell a leaf of zeros that came from a hole from one of written zeros.

### A fragment flag on content stored with holes

The root fragment of content stored with holes could carry a new flag bit, so a reader could tell
such content from its root, without walking to a hole.

*Rejected because:* it moves no refusal and breaks the servers this change keeps working. A reader
built before this change does not check a flag bit it gives no meaning, as its check that a
fragment is consistent states, so it would still refuse the content at the first hole it walks, not
at the root. A server built before this change refuses a fragment whose flags hold a bit it does
not know, at ingress, so every put of content with holes to it would fail, where without a flag it
stores all such content but content that is one hole. What a flag would buy is a check that needs
no load of the root, such as refusing content stored with holes when a file is added to a revision
tree or modified. That can be weighed again with the follow-up that lets revision trees hold such
content, which needs a server upgrade anyway.

## Prior Art

- GNU tar's sparse archive formats record the ranges of data of a file and the file's size, and
  recreate the holes on extraction; the hole references here record the same thing in a list.
- rsync's `--sparse` writes runs of zeros as holes on the receiving side. This proposal does not
  take zeros for holes, so zeros written to a file come back as written data.

## Future Work

This is the initial implementation, for Linux file systems that report holes. Planned follow-up
work extends it:

- Windows, on NTFS and ReFS: find the ranges of data with
  `FSCTL_QUERY_ALLOCATED_RANGES`, mark the file a get writes sparse (`FSCTL_SET_SPARSE`) before
  sizing it so its holes stay unallocated, and unmap ranges with `FSCTL_SET_ZERO_DATA` where a write
  over an existing file needs it. Until then a put on Windows finds no holes and stores the whole
  file as data, and a get there leaves holes unwritten in a file that is not sparse.
- macOS, on APFS: holes found and recreated there, where the put finds the ranges of data with
  `SEEK_DATA` today.
- Other file systems whose holes the `SEEK_DATA` fallback reports less precisely than an extent
  map.
- Revision trees holding content stored with holes: FUSE reading a hole as zeros, and status
  comparing a file's holes as well as its data.

The resumable get, a separate change that keeps the `<path>.loretmp` of a get across a failed get
and hashes each leaf already in it on the retry, builds on this one. It must not empty a temporary
file it resumes, and it counts a hole's range as held, as a hole has no leaf to hash.

## Unresolved Questions

None.
