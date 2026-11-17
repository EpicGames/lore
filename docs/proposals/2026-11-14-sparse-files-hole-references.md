---
lep: 2026-11-14-sparse-files-hole-references
title: Hole references for sparse files in fragment lists
authors:
  - Mattias Jansson
status: Accepted
created: 2026-11-14
updated: 2026-11-17
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
unwritten, so the file it writes has the same holes, in the units its file system allocates, where
that file system supports holes. The initial implementation is for Linux file systems that report
holes. macOS on APFS, and Windows on NTFS and ReFS, are additional work after it, each taking holes
as those file systems record them.

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
- A get into a file recreates the holes, so the file it writes is allocated like the source, in
  the units its file system allocates.
- The fragment list of a sparse file grows with the number of ranges of data, not with the file's
  size.
- Content stored without the new field, and every existing reader of it, is unchanged, but for the
  file a storage API get writes on Windows, which is sparse ([Get](#get)).
- No reader built before this change returns wrong content for content stored with holes.

### Non-Goals

- Turning zeros into holes. Zeros written to a file are data, and stay data wherever the file
  system records them as allocated ([What a hole is](#what-a-hole-is)).
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

A hole is a range the file system records as holding no written data: a range never written, which
it maps to no storage, or an extent it records as allocated and never written (an *unwritten*
extent, such as one `fallocate` reserves). Zeros written to a file are data. A file with a hole
and the same file with zeros written over it are different content, with different addresses,
unless the file system leaves those zeros unallocated: then it has already made them a hole, and
they are one, as the put never reads content to decide what is a hole. A file with no hole gets
the same address with or without `skip_holes`, and a file larger than one fragment that has holes
gets a different one.

On Linux the put finds the ranges of data in the file's extent map, from the `FIEMAP` ioctl with the
flag that syncs the file first. Written extents are data; unwritten extents and gaps between extents
are holes. What the page cache holds does not change the result, so the same file always gets the
same address. Where a file system has no extent map, as tmpfs, the put falls back to `lseek` with
`SEEK_DATA` and `SEEK_HOLE`, which takes an unwritten extent as data wherever the page cache holds
pages for it. On its first `SEEK_DATA` lookup the put tells the system that it reads the source at
random, so it reads nothing ahead into a hole and leaves no pages there.

#### macOS (APFS)

macOS on APFS is additional work after the initial Linux implementation. APFS reports the holes of
a file, but when it writes a file's data out, it allocates as zeros the holes beside that data that
are smaller than a size of its own. A small hole beside data is therefore a hole only until APFS
writes that data out, and allocated zeros after.

The put has the file's data written out before it looks for holes, as the put does on Linux with
an extent map, so what is a hole depends on what the file system allocated alone, and the same
file gets the same address whether or not APFS had written its data out before the put. A small
hole APFS has allocated is data to the put.

The get keeps the holes of the content holes: once it has written every range of data, it has that
data written out, then deallocates each hole, before it completes. It deallocates only the ranges
of holes, never a range of data, so written zeros stay written. A part of a hole in a block the
file system shares with data stays allocated as zeros, since the file system allocates whole
blocks. Linux needs no such step, as a range left unwritten stays a hole there. A get that cannot
deallocate a hole on a file system that holds holes fails rather than complete with the hole
allocated; on a file system that holds none, it completes, and its holes read as zeros.

A resumed get on APFS finds the small holes of its temporary file allocated the same way. Where
the temporary file's file system holds holes, data in the whole blocks of a hole does not make the
get write the file afresh, since the get deallocates those blocks before it completes. Bytes other
than zeros in a block a hole shares with data still do, as the get does not deallocate that block.

#### Windows (NTFS, ReFS)

Windows support is additional work after the initial Linux implementation. It takes holes as NTFS
records them, and on ReFS the holes a sparse file's allocation shows. NTFS records, for each file,
its valid data length, where what was written to the file ends as the file system tracks it, and
for a sparse file, the ranges it records as allocated; it keeps no record of which allocated ranges
below the valid data length were written. Windows support measured this on NTFS with clusters of
4 KiB. It also measured ReFS 3.14 with clusters of 4 KiB. Once a sparse file's data is written
out, ReFS records every range written to the file as allocated ([Compatibility](#compatibility)),
which the put takes ReFS to do ([Risks and Assumptions](#risks-and-assumptions)). ReFS 3.14
reports a file cut short and grown again past what was written as valid to its end, so the put
reads no valid data length there.

- On NTFS, everything at or past the valid data length was never written, and is a hole, as an
  unwritten extent is on Linux, whether the file is sparse or not. A file extended past what was
  written, or preallocated and then made sparse, stores that tail as a hole.
- Below it, a range a sparse file records as not allocated is a hole, and every range it records as
  allocated is data, the part of an allocation unit that no write reached included, which reads as
  zeros. A file that is not sparse records every range as allocated, so it is data below its
  valid data length.
- Written zeros below the valid data length are data where the file system records them as
  allocated, including where it stores them compressed into no storage, as NTFS does. Where the
  file system itself leaves them unallocated, as NTFS can for a small write of zeros alone in an
  allocation unit of a sparse file, they are a hole: the file system has already made them one, and
  the put never reads content to decide what is a hole.
- The put sees the file's data written out, as on Linux, so data written through a memory mapping
  and not yet written out is data, not a hole. The put takes the valid data length from NTFS
  alone, and only where NTFS reports a single valid data length for the file. On ReFS and any
  other file system, and where NTFS reports none that way, the ranges the file records as
  allocated alone decide: a sparse file keeps its unallocated holes, and a file that is not sparse
  stores the tail past what was written as data. Windows support takes ReFS's allocated ranges so
  that sparse images on ReFS, which this work was filed for, keep their holes. Seeing the data
  written out matters on ReFS as on NTFS: before it, ReFS 3.14 left written data out of the
  ranges it recorded in some of the files measured, and after it in none.
- Where the file system cannot report the ranges a sparse file records as allocated, the file is
  data below its valid data length, and data whole where the put takes no valid data length.
- Where the put cannot see the file's data written out, as for a file that is read-only or that
  the caller may not write, or while another process holds it open without sharing write access,
  the whole file is data, with no hole: data written through a mapping and not yet written out is
  neither within the valid data length nor allocated, as measured on NTFS, and the read-only
  attribute binds only handles opened after it is set, so one opened to write before it still
  writes.
- A file whose data its file system may keep elsewhere until it is read, as a cloud placeholder, an
  offline or tiered file, or one behind a reparse point, is data whole, with no hole: what it
  records as allocated, or as written, need not hold its data. A file is judged by what it reports
  itself, so a symbolic link is judged by the file it leads to.

None of this applies to a put without `skip_holes`, which reads the file whole.

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
range starts unallocated, unless it resumes from that file, as below. The file is the
`<path>.loretmp` the get stages through, or the target itself for a direct write, which replaces
the whole file. A hole is left unwritten, so it is a hole of the written file on a file system that
supports holes, and holds none of what that file held before. Every range of data is written,
zeros included. Holes in the source come back as holes, and the get writes every zero stored as
data back as data. A put without `skip_holes` stores holes as data, which a get writes back as
data. A path takes one get at a time: items that name the same path, in one call or in concurrent
calls, are not supported.

A get that resumes from the `<path>.loretmp` an earlier get of the same content and range left
keeps its holes as a fresh get does. It counts a hole as held, as a hole has no leaf to fetch. It
writes the file afresh, with every range unallocated again, as when it empties a temporary file of
other content, when any of these holds: an allocation unit wholly inside a hole of the content is
allocated in that file, the part of a unit a hole shares with data holds bytes other than zeros,
or its holes cannot be checked. The file it keeps then holds the holes a fresh get leaves: the parts of a
unit beside data, which a write of that data allocates, hold zeros.

A storage API get on Windows writes content stored as a fragment list into a sparse file, so each
hole it leaves unwritten holds no storage there; on a file system that cannot make a file sparse,
the file holds a hole as zeros, which read the same. It empties the file it writes so that none of
what the file held stays allocated: ReFS 3.14 records the ranges a sparse file held past a cut as
allocated again once it grows, where its data was not written out before the cut
([Compatibility](#compatibility)), and a get that releases the file's storage whole before it
cuts it leaves none of them. Where that release fails, the cut alone empties the file, and a hole
of the content can hold allocated zeros there, which read the same. A get the revision layer makes
does not mark its file sparse, as a revision's files hold no holes, and writes it as before
Windows support: a direct write over an existing file keeps that file's sparse attribute, whatever
made it sparse. NTFS allocates a sparse file in units of several clusters
([Compatibility](#compatibility)), so a hole comes back as a hole in whole units: the parts of a
unit beside data are allocated, and read as zeros.

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
  leave holes unwritten. They empty a `<path>.loretmp` an earlier get left before writing it, but
  for one of the same content and range, which they resume from ([Get](#get)). On macOS a put
  with `skip_holes` has its source's data written out before it looks for holes, and a get of
  content with holes deallocates them once it has written its data, and resumes from a temporary
  file whose holes the file system allocated; no signature or layout changes. On Windows they
  write content stored as a fragment list into a sparse file, whether it holds holes or not: the
  file carries the sparse attribute, and a direct write over an existing target leaves it sparse
  too. Nothing a caller reads from the file changes; what tools report of it does: the sparse
  attribute, and its allocation, in the file system's units. On a file system that cannot make a
  file sparse, the file is written as before. A hole in fragment delivery is a `GET_FRAGMENT` with
  no payload. No CLI command changes.
- **Addresses on Windows** — A put with `skip_holes` on Windows found no hole before Windows
  support, and stored every file whole. The same file put again with the field can get another
  address, where it holds holes. Content stored before keeps its address and reads as before, and
  every reader built with hole references reads both, so no stored content needs migrating;
  readers upgrade first ([Migration Plan](#migration-plan)).
- **File systems** — Measured on Linux on four file systems: ext4, XFS and btrfs, where the put
  finds holes through `FIEMAP`, and tmpfs, which has no extent map, where it finds them through
  `SEEK_DATA` and `SEEK_HOLE`. On each, an image with holes put with `skip_holes` and got back read
  as the image, byte for byte, with only its data allocated; a file with zeros written into a
  range came back with that range allocated, as data; and a range `fallocate` reserved and never
  wrote was stored as a hole and came back unallocated. Measured on macOS on APFS, with blocks of
  4 KiB: the file system reports every hole exactly until it writes the data beside it out, then
  allocates as zeros the holes under 16 MiB beside that data (a gap of 16380 KiB filled, one of
  16384 KiB stayed a hole), and a hole deallocated after that stayed a hole when the file was
  written out again. An image with holes put skipping them and got back read as the image, byte for
  byte, with the whole blocks of every hole of the content unallocated; a file put before APFS
  wrote its data out got the address it got after; written zeros came back allocated; and a resumed
  get of content with small holes loaded only the leaves its temporary file lacked and kept every
  hole. A file put with `skip_holes` on macOS by a build before this amendment, which did not have
  its data written out first, may have stored small holes as holes, and so have another address
  than the one it gets now; it reads back the same either way.
- **File systems on Windows** — Measured on Windows Server 2025, in a process-isolated container,
  on NTFS with clusters of 4 KiB, through the unit tests of the walk and of the put and get: a
  sparse file put with `skip_holes` and got back read as the file, with only its allocated ranges
  allocated, in units of 16 clusters; a byte written into a sparse file was reported as data of
  its whole unit, the part no write reached included; a file that is not sparse was reported as
  data to its valid data length; a file that is not sparse, extended past what was written, and a
  file preallocated and then made sparse, stored the tail past the valid data length as a hole and
  came back as the file, with that tail unallocated; a file written through a memory mapping not
  yet written out stored no hole, and so did a sparse file the put could not write out; a walk of
  a read-only sparse file, and of one marked offline, found no hole; written zeros came back
  allocated, as data; and a resumed get that found data in a hole wrote the file afresh with that
  hole unallocated. That the put trusts no other report of a valid data length than a single one
  for the file was checked with reports built by hand, as NTFS makes no other. Measured by hand on
  the same host, zeros written to a compressed file took no storage and were still reported
  allocated, so they stay data. Measured outside a container, on Windows 10.0.26200 Enterprise,
  NTFS left a write of 4 to 17 zero bytes alone in an otherwise unwritten 64 KiB allocation unit
  of a sparse file unallocated once it was written out. A put walking that allocation stores it as
  a hole, and a get leaves a hole there; it reads as zeros either way. Runs of 4 MiB of written
  zeros stayed allocated, as above; Windows support measured no run of a size between. Windows
  support did not measure the fallbacks where the valid data length or a sparse file's allocated
  ranges cannot be read, or the file system cannot make a file sparse.
- **ReFS** — Measured on Windows 10.0.26200 Enterprise, on ReFS 3.14 with clusters of 4 KiB. Every
  unit test that puts a sparse file with `skip_holes` and gets it back found it read back as the
  file. A program that writes files, has their data written out and reads their allocated ranges
  as the put's walk does wrote about 10,000 sparse files and 27 GiB with no antivirus filter in
  the path of the writes, and about 10,000 files and 25 GiB again with one. In every file, every
  byte written was within the ranges ReFS recorded as allocated once the data was written out,
  written zeros included, and a byte written alone was recorded as one cluster. The writes were
  cached, written through, unbuffered, made through a memory mapping not yet written out, or
  cloned from the blocks of another file, with ranges deallocated and files grown and shrunk.
  Before the data was written out, ReFS left written data in the first cluster of 1 to 9 files per
  seed out of the ranges it recorded. ReFS recorded ranges no write reached as allocated, reading
  as zeros and never as bytes the file held, in two cases. A sparse file cut short before its data
  was written out and grown again recorded the ranges past the cut that were written before it,
  through any handle, also after a later write-out and 5 to 120 seconds after the write. A file
  written through a memory mapping still open recorded the 64 KiB from each page written, until
  the mapping closed. Releasing a file's storage whole before cutting it left no range past the
  cut. ReFS reported a file cut short and grown again past what was written as valid to its end,
  in one region. A file created, marked sparse and sized through one handle, and never written,
  recorded nothing as allocated, and held one cluster of storage.

## Non-Functional Considerations

- **Concurrency** — Preserved on Linux. A put reads one range of data at a time, as it reads a file
  today. A get writes the leaves of the ranges of data concurrently, as today, and writes nothing
  for a hole. On Windows a put with `skip_holes` changes what another process sees: to see a
  file's data written out, it holds the file open to write for a moment, sharing every access, and
  writes nothing to it, so a process that opens the file then without sharing write access is
  refused, as it is while any other process writes the file. Windows support accepts that, as
  without it data written through a mapping could be stored as a hole. A file another process
  holds open without sharing write access is put with no hole while that process holds it, so it
  can get another address than it gets after.
- **Memory** — Preserved. A hole costs one reference in a list and nothing in memory. A put keeps
  one window of a range in memory, as it keeps one of a file, and a streamed read delivers a hole in
  pieces no larger than a fragment, from memory shared by every read. On macOS a get of content
  with holes keeps the range of each hole until it completes, to deallocate them once its data is
  written out: a fixed cost per hole, so the memory grows with the number of holes, which the
  fragment list bounds.
- **Statelessness** — Preserved. The change keeps no state across operations.
- **Determinism** — The address of a file put with `skip_holes` depends on how the file is
  allocated as well as on what it reads as, by design. On Linux, on a file system with an extent
  map, the same allocation always gives the same address, whatever the page cache holds. On macOS
  the same holds, as the put has the file's data written out first. On Windows the address follows the valid data length and the ranges the file records as allocated.
  On a file system other than NTFS, and where NTFS reports no valid data length that covers the
  file from its start, the address follows the allocation alone, so a file whose tail past what was
  written is allocated gets another address than the same file on NTFS. On ReFS a sparse file cut
  short before its data was written out and grown again, or one written through a memory mapping
  still open while it is put, can record as allocated ranges no write reached, so it gets another
  address than the same bytes written otherwise. Where the file system cannot report a sparse
  file's allocated ranges, the file is data below its valid data length. Where the put cannot see
  the file's data written out, or the file's data may be held elsewhere, the file is stored with
  no hole, so a file with holes gets another address than when the put sees its data written out.
  As NTFS allocates in units of 16 clusters on the 4 KiB clusters measured, the same bytes written
  on NTFS and on a file system that allocates in smaller units can get different addresses, so
  content put on Linux, got onto NTFS and put again there can get a new one. So can content whose
  data includes a small write of zeros alone in an allocation unit, which NTFS can leave as a hole
  where Linux holds it as data. Content put without the field is unchanged.
- **Latency** — On macOS a put waits for the file's data to be written out before it looks for
  holes. For a file APFS has already written out this costs nothing measurable; for one just
  written it costs the time APFS takes to write that data out, which it would spend anyway.
  Measured on CI's macOS runner, a put of a 256 MiB file holding 64 MiB of data just written took
  2.3 s, against 0.3 s once APFS had written that data out. A get of content with holes on macOS
  waits, every time, for the data it wrote to be written out before it deallocates the holes,
  where a get on Linux does not wait; that cost is not measured.
- **Throughput** — Measured on XFS, on a shared machine whose disk was busy at times, and on NTFS
  only for a get, which bounds nothing:
  - The generated 8 GiB image, with 2.1 GiB of data in 235 ranges. With `skip_holes` a put reads
    and hashes 2.1 GiB into lists of 364 KB (8,853 leaves and 236 holes); without, 8 GiB into lists
    of 1.31 MB (32,768 leaves). Without contention the put took 3.7 to 3.9 s and 6.4 to 7.0 s of
    CPU, against 3.6 s and 10.4 s, both bounded by writing the 2.1 GiB of new fragments to the
    store. A get took 0.6 to 1.0 s and allocated 2.1 GiB, against 4.2 s and 8 GiB.
  - The 100 GiB XFS image. With `skip_holes` a put reads 8 GiB into 32,797 references, against
    100 GiB into 409,600, and a get allocates 8 GiB, against 100 GiB.
  - On NTFS Windows support established no throughput, and timed no put. On the shared container
    host it was measured on, a storage API get of a 4 GiB file of random data, with no hole,
    took 6 to 112 s into a sparse file and 10 to 529 s into one that is not, over six runs of each
    in two orders, the slowest always the first to run. The runs varied too much to bound
    anything.

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

On Windows, a caller that already set `skip_holes`, and found no hole there, starts storing content
with holes once it runs a build with Windows support, so every client that reads its content
upgrades to a build with hole references first. The sparse get on Windows has no field to turn it
off: a rollback to a build without Windows support stops it, and the files already written sparse
stay sparse, and read the same.

## Security Considerations

The trust model is unchanged. A crafted list can claim a hole of any size within the content size
its root fragment states, as a crafted list can already claim content of that size; a get sizes the
file it writes to the content size as before, and a hole only leaves part of it unwritten. The
server checks the rules above on every list it is given, and every walk treats a list that holds a
hole as a list of leaves, refusing a reference in it that spans more than a fragment or names a
list. A hole names no fragment, so it reaches no stored data. No new data is integrity- or
confidentiality-sensitive. On Windows a put with `skip_holes` asks for write access to the file it
puts, to see its data written out, and writes nothing to it; where it is refused, the put stores
the file whole.

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
- **Assumption:** on APFS a hole the get deallocated stays a hole until the file is written again
  — *invalidated if:* APFS allocates such a hole on its own later, so the file a get wrote holds
  zeros there rather than a hole; it still reads as the content.
- **Assumption:** ReFS records every range written to a sparse file as allocated once the file's
  data is written out, in units of one cluster, as measured on ReFS 3.14 with clusters of 4 KiB
  ([Compatibility](#compatibility)) — *invalidated if:* another version of ReFS, another cluster
  size, a sealed ReFS sparse VHDX ([Future Work](#future-work)), or a way of writing the
  measurement did not cover records otherwise. Each way it can differ costs this:
  - it records a written range as not allocated: a put there stores written data as a hole, and a
    get returns zeros for it;
  - it allocates in larger units: a resumed get writes its file afresh more often than it needs
    to.

**Risks**

- **Risk:** a sparse file a storage API get writes on Windows fragments, or writes more slowly,
  than one that is not, which Windows support could not measure; at worst NTFS refuses to grow a
  file fragmented past what it can record, and the get fails with an error —
  *mitigation:* the NTFS measurement in [Future Work](#future-work) bounds it, and a slower,
  fragmented or failing storage API get on Windows is the signal to roll the sparse get back
  ([Migration Plan](#migration-plan)); until then it is accepted for every file the storage API
  writes, those that hold no hole included
  ([Alternatives Considered](#alternatives-considered)). The revision layer does not mark its
  files sparse, as they hold no holes.
- **Risk:** a file system that stores written zeros as holes, as some do when they compress, and as
  NTFS can for a small write of zeros alone in an allocation unit of a sparse file, reports them
  as holes, and the put records holes there — *mitigation:* accepted; the file system has already
  turned those zeros into holes, and the get writes them back as holes.
- **Risk:** ReFS records ranges no write reached as allocated, as measured on ReFS 3.14: for a
  sparse file cut short before its data was written out and grown again, and for the 64 KiB from
  each page written through a memory mapping while the mapping is open — *mitigation:* accepted;
  the put stores those ranges as data, as ReFS records them allocated, and they read as zeros,
  never as bytes the file held, so it costs holes, never data. A storage API get on Windows
  releases the storage of the file it writes whole before it cuts it, which left no such range
  where measured.
- **Risk:** a client built before this change meets content stored with holes — *mitigation:* it
  refuses the content with an error on every path that reaches a hole, as measured above, and the
  Migration Plan upgrades readers first.

## Drawbacks

- The same bytes can have two addresses, one with holes and one without, so a file stored both ways
  is stored as two lists. Fixed-size leaves inside its data have the same addresses either way, and
  still deduplicate.
- Content stored with holes is unreadable by every client built before this change.
- On macOS, APFS allocates the small holes beside data when it writes that data out, so the put
  stores them as data, and an image whose holes are small saves little there.

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

### On Windows, holes from the allocation alone

A put on Windows could take as holes only the ranges a sparse file records as not allocated.

*Rejected because:* a file that is not sparse records every range as allocated, so a file extended
or preallocated past what was written would store that tail as data, read and hashed as zeros,
where Linux stores an unwritten extent as a hole. The valid data length gives NTFS the same rule;
ReFS 3.14 reports a file grown past what was written as valid to its end, so the put takes the
allocation alone there, which costs holes, never data.

### On Windows, a sparse file only for content that holds a hole

A storage API get on Windows could mark its file sparse only where the content holds a hole.

*Rejected because:* the get learns whether the content holds a hole only as it walks the list,
after it has sized the file, and a file sized before it is marked sparse holds its unwritten ranges
as allocated, measured on NTFS. Deciding before sizing would mean loading every sublist of a list
before the first write, a load per sublist before any leaf is fetched, where the walk now loads
them as it writes. Marking the file sparse at the first hole and releasing the allocation of the
holes already sized is rejected for the reason a get never punches holes: it releases what its
file held once, before it writes, and leaves holes unwritten, so no hole holds an allocation to
release, and releasing each adds a call per hole.

## Prior Art

- GNU tar's sparse archive formats record the ranges of data of a file and the file's size, and
  recreate the holes on extraction; the hole references here record the same thing in a list.
- rsync's `--sparse` writes runs of zeros as holes on the receiving side. This proposal does not
  take zeros for holes, so a get writes zeros a put stored as data back as data.

## Future Work

Planned follow-up work extends this design:

- ReFS measured on a sealed ReFS sparse VHDX, on versions of ReFS other than 3.14, and on volumes
  with clusters larger than 4 KiB.
- NTFS measured outside a container: the throughput of a put and a get, the fragmentation of the
  sparse files a get writes, and volumes with clusters larger than 4 KiB. That measurement comes
  before revision trees hold content stored with holes, below, as their files would then be
  written sparse.
- Other file systems whose holes the `SEEK_DATA` fallback reports less precisely than an extent
  map.
- Revision trees holding content stored with holes: FUSE reading a hole as zeros, and status
  comparing a file's holes as well as its data.

## Unresolved Questions

None.
