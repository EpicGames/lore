# The Lore Version Control System

> **Status:** Draft

A reference document explaining the rationale, goals, and theoretical design of Lore. This is not a
peer-reviewed paper; it is a public-facing standalone description of the system, intended to make
the *why* of Lore - the problems it was built to solve and the design choices that follow from those
problems - as legible as a reader can make it without reading the source code.

---

## Contents

1. Introduction
2. Motivation: Why a New VCS?
3. Goals and Non-Goals
4. Conceptual Model
5. Architecture Overview
6. API-First Design
7. Content-Addressed Immutable Storage
8. Chunking and Fragmentation
9. The Mutable Store
10. Revisions and the Merkle Tree
11. Metadata
12. Sparseness and Partial Working Copies
13. Binary-First by Design
14. Centralized with Offline Capability
15. State Detection and Staging
16. Fault Tolerance and Atomicity
17. Access Model, Data Isolation, and Multi-Tenancy
18. The Storage Protocol
19. Obliteration and Data Lifecycle
20. Replaceable Backends
21. Backend Scalability
22. Sub-Repository Links
23. Layers
24. Shared Stores and Instances
25. Comparison with Prior Art
26. Open Problems and Future Work
27. References

---

### 1. Introduction

This section sets out what Lore is, who the document is written for, and the recurring
terminology the rest of the document depends on.

#### 1.1 What Lore is

Lore is a centralized version control system designed to scale along every axis - number of files,
size of files, depth of history, number of branches, number of concurrent users, and number of
repositories sharing a backend - without privileging any particular kind of content. All data is
treated as opaque byte streams; text and binary data flow through the same primitives.

Structurally, Lore is two systems: a *storage subsystem* - a partition-based, content-addressed
store that deduplicates all content while enforcing strict per-partition access boundaries, usable
entirely on its own through its own public API - and a *version control subsystem* that builds
revisions, branches, merges, and staging out of storage primitives. Version control is one consumer
of the storage API, not a layer with privileged access to it.

The storage subsystem is built from two stores. Every piece of content is stored once in an
immutable, content-addressed data store keyed by BLAKE3 hashes. Mutable state, like branch pointers
and other small bookkeeping, lives in a separate, smaller key-value store. The version control
subsystem uses both: a repository is a Merkle tree of files and directories whose nodes and content
live in the immutable store, while branch pointers and name lookups live in the mutable store.

Files larger than a threshold are split into chunks, content-defined via FastCDC or fixed-size
depending on file type, so that a single edit inside a multi-gigabyte file re-uploads only the
changed chunks and any byte range can be read without materializing the whole file. Clients hold
only the data they actively use; the rest is fetched lazily from the remote - a deployment of one or
more server processes with optional cache and replica tiers - where strict per-repository access
boundaries (*partitions*) make the system safe to run as a multi-tenant service. A canonical
implementation is provided as a Rust library, server, and CLI, but the data formats and wire
protocols are publicly specified and versioned, so other implementations can read, write, and serve
Lore data.

#### 1.2 Audience and reading paths

This document is for engineers evaluating Lore for a workload, building tools or backends against
it, or maintaining a deployment. It assumes general familiarity with version control concepts
(committing, branching, merging, rebasing) and with content-addressed storage in the style of Git.
It does not assume any Lore-specific knowledge.

Suggested paths through the rest of the document:

- *Why Lore exists and what it commits to:* §2 motivation, §3 goals, §4 conceptual model.
- *The data plane:* §5 architecture, §6 API-first discipline, §7–8 immutable storage and chunking,
  §9–10 mutable store and revisions, §11 metadata, §12–13 sparseness and binary-first.
- *The control plane:* §14 centralized + offline, §15 state detection and staging, §16 atomicity,
  §17 access model and multi-tenancy, §18 the storage protocol.
- *Lifecycle, deployment, and scale:* §19 obliteration, §20 replaceable backends, §21 backend
  scalability.
- *Composition:* §22 links, §23 layers, §24 shared stores and instances.
- *Comparison and remaining work:* §25, §26.

This document is the conceptual spine. Byte-level format documentation, command reference, and
exhaustive edge-case discussion live elsewhere.

#### 1.3 Terminology

Recurring Lore-specific terms used throughout this document. Each is defined again in context where
it first becomes load-bearing.

- **Repository.** A versioned tree of files and directories under a single access boundary. In the
  storage layer, one repository corresponds to one *partition*.
- **Partition.** A 16-byte opaque identifier that partitions content in the storage layer.
  Authorization binds a session to a partition; content lookups are partition-scoped. Partitions are
  the access boundary.
- **Remote / backend / deployment.** The collective remote environment for a repository - one or
  more Lore server processes, edge caches, read replicas, and storage tiers, operated as a unit. The
  terms *remote*, *backend*, *deployment*, and (when unqualified) *a server* are used
  interchangeably throughout this document. An individual server process is named explicitly when
  the distinction matters.
- **Instance.** A local working directory of a repository, identified by a UUIDv7 stored in
  `.lore/instance`. Multiple instances of the same repository can coexist on a machine, sharing
  a single shared store while keeping independent working trees, views, staged state, and
  current branches.
- **Shared store.** A single on-disk immutable + mutable store referenced by one or more
  instances on a machine. Multiple instances over the same shared store share fragment storage
  and cached content but maintain independent per-instance state.
- **Storage subsystem.** The combination of the immutable content-addressed store and the mutable
  key-value store, exposed as a first-class public API. Independent of, and used by, the version
  control subsystem.
- **Version control subsystem.** Revisions, branches, merges, staging, sync, push, diff, query -
  the version control layer built on top of the storage subsystem and exposed through its own public
  API.
- **Hash.** A 32-byte BLAKE3 hash of a content payload. Lore's address function throughout.
- **Context.** A 16-byte opaque tag carried alongside a content hash. Used for tracking identity
  (e.g. file ID for move/copy and obliteration scope). Context is *not* an access boundary, it is a
  deduplication construct.
- **Address.** The pair (hash, context), 48 bytes, used to identify a fragment in the immutable
  store.
- **Fragment.** A unit of content stored in the immutable store, addressed by hash. A fragment can
  hold a raw payload or a list of references to other fragments (recursive fragmentation).
- **Fragment reference.** An entry in a fragment list, recording the hash of a child fragment and
  the byte offset it represents in the reassembled content.
- **Immutable store.** The content-addressed data store. Append-only; entries are added or
  obliterated, never modified.
- **Mutable store.** The key-value store holding non-content-addressed state, such as branch
  pointers and other small bookkeeping.
- **Revision.** A frozen snapshot of the repository tree, identified by the hash of its serialized
  state. Revisions form a graph; each has one parent, or two on a merge.
- **Branch.** A named, mutable pointer to a *latest revision* - Lore's analogue of Git's HEAD.
  Branch state lives in the mutable store and may differ between a client and the remote, or
  between two separate remotes.
- **Node / node block.** Fixed-size record (a file or directory entry) and the fixed-size container
  that groups them. The serialization unit of the Merkle tree.
- **View** (`.lore/view`). Inbound filter declaring the sparse subset of the repository materialized
  to disk.
- **Ignore** (`.loreignore`). Outbound filter declaring paths excluded from staging and committing.
- **Stage.** Recorded intent to include a set of file changes in the next revision. Staging does not
  produce fragments; committing does.
- **Dirty.** A flag on a tree node indicating that the file at that path differs from the committed
  revision. Orthogonal to *stage*: a file can be dirty, staged, or both.
- **Staged anchor.** A per-instance pointer in the local mutable store to the state tree that
  records the instance's divergence from the committed revision. The state tree itself is
  content-addressed in the immutable store and persists both dirty and staged flags; neither the
  anchor nor the tree is ever transmitted to the remote.
- **Link.** A reference from one repository to a specific subset and revision of another, mounted at
  a path. Each linked repository is its own partition with its own access control. Links are part of
  the committed revision: every clone of a revision sees the same links.
- **Layer.** An overlay of a subset of one repository's content into another at a path, applied
  locally rather than stored in the revision. Layers do not travel with a revision; different
  machines can have different layer configurations.
- **Obliteration.** Removal of a fragment's payload while preserving its address. Reads of an
  obliterated fragment return a typed absence rather than corruption.

### 2. Motivation: Why a New VCS?

Lore exists because no widely deployed version control system combines the properties demanded by
the largest active development workloads: arbitrary content type, multi-axis scale, multi-tenant
safety, a public versioned specification, and a permissive open-source license. The motivation is
not that prior systems are bad - they are mature and excellent at what they were designed for - but
that none of them were designed for the union of constraints Lore targets.

#### 2.1 The workloads Lore was built to handle

The motivating workloads share three properties.

They are *content-agnostic*. A typical repository holds source code, build inputs, configuration,
prebuilt artifacts, large data files, generated content, and arbitrary binary blobs. No single
content shape dominates, and no useful tool can specialize to one shape and treat the rest as a
degraded case.

They are *large on every axis*. File counts in the millions, individual files in the terabyte range,
histories with millions of revisions, hundreds of branches per project, thousands of concurrent
users, and hundreds of repositories sharing a single backend deployment. Any one of these axes
pushes existing systems out of their comfort zone; in combination they leave no candidate.

They are *centrally coordinated*. A single logical source of truth must exist - for access control,
durability, audit, and conflict resolution - but developers must still be able to work offline,
queue revisions, and stage changes without a round-trip to the remote. The source of truth is a
logical role, not a single host: in a Lore deployment it can be one server or a fleet of servers
behind cache and replica tiers.

#### 2.2 What existing systems do well

A short and unsentimental survey of the systems Lore most resembles.

*Git* is the dominant distributed VCS. Its content-addressed object store, three-way merge, branch
model, and ecosystem are excellent, and its commit graph is the closest existing analogue to Lore's
revision graph.

*Perforce* is the canonical centralized VCS for large-content workloads. Its server-of-record model,
file-level locking, and stream architecture are the prior art for Lore's centralized design.

*Mercurial* and its descendant *Sapling* solved monorepo scale on the commit-graph and
tree-traversal axes, with sparse and lazy data fetching and a UX (Smartlog, commit stacks) that is
excellent.

#### 2.3 Where existing systems fall short for these workloads

*Git* has moved in the directions Lore cares about. Partial clone (`--filter=blob:none`) combined
with sparse checkout gives a real lazy-fetch workspace, and the wire protocol is formally versioned
(`protocol.version=2`, with capability negotiation). The remaining gaps matter for the workloads
above: multi-gigabyte files are still handled out of band via LFS rather than as first-class
content; sparse checkout's sub-directory (file-pattern) granularity exists only in the deprecated
non-cone mode, with cone mode pinned to directory boundaries; the sparse-and-partial combination is
still flagged experimental and has sharp edges in offline use, since commands that reach for blobs
outside the sparse specification attempt on-demand fetches and fail without a server; deduplication
of binary content is bounded by pack delta heuristics; and multi-tenant hosting is left entirely to
adjacent infrastructure.

*Perforce* requires server round-trips for normal operations: opening a file for edit, listing
changes, syncing. Offline work is possible but awkward, and reconciling out-of-band changes is a
manual process. Storage is delta-encoded RCS for text and full-file for binaries; native
deduplication is limited to "lazy copies" at branch creation time and whatever the underlying
storage system provides. MD5 is still the integrity hash, which is no longer a contemporary choice.
The protocol is proprietary, which forecloses third-party reimplementation.

*Mercurial* and *Sapling* have solved the scale of source-shaped repositories elegantly, but text
remains the primary citizen: line-oriented merges, text-shaped diffs, and evolve workflows assume
textual history. Neither is designed for repositories whose dominant content is binary, nor for
multi-tenant deployments where strict isolation between repositories is a requirement.

The spec contract is partial in each case. Git's wire protocol v2 is publicly versioned and well
documented, but on-disk pack formats, storage backend interfaces, and authorization contracts are
not contracted at the same level; Mercurial is similar; Perforce is proprietary throughout. None of
the three treats the entire stack - on-disk formats, wire protocols, storage interfaces,
authorization model - as a single versioned specification independent of its canonical
implementation.

#### 2.4 The gap Lore is filling

What Lore offers that the prior art does not is the union: content-addressed integrity, centralized
server-of-record durability, sparse and lazy data fetching at every granularity, fragment-level
deduplication that is as effective on a multi-gigabyte binary as on a kilobyte of text, multi-tenant
isolation by design, and a publicly specified wire and storage format. Each of these exists
somewhere in the prior art; no production system combines all of them.

Equally important, Lore is open source under the MIT license. The combination of these properties
under a permissive license is what makes Lore worth building rather than working around the gaps in
something that already exists.

### 3. Goals and Non-Goals

Lore commits to a specific set of properties and explicitly excludes others. The remainder of this
document is in service of these commitments; this section names them.

#### 3.1 Design goals

**Binary-first.** All content is treated as opaque byte streams on the hot path. Text-aware features
are layered on top, never assumed by the storage or transport paths. Binary content gets the same
first-class treatment as text.

**Centralized but offline-capable.** The remote is the source of truth for durability, access
control, and conflict resolution. Normal client operations (staging, committing, branching,
switching, diffing) never require a round-trip.

**Sparse by construction.** An instance materializes only the subset the user has asked for;
clients fetch only the fragments they need, on demand. The cost of an operation tracks the working
set, not the repository.

**Atomic state.** Every operation either completes fully or leaves the repository in its
pre-operation state. There are no partially applied revisions, no half-pushed branches, no
in-between repository states visible to readers.

**Cryptographically verifiable.** Every fragment is identified by its BLAKE3 hash; the revision
graph is hash-chained; the server validates every push end-to-end before advancing the latest
pointer. Tampering and corruption are detectable.

**API-first.** The C library and its public header are Lore's primary artifact. Both subsystems are
exposed through it as first-class APIs: the storage subsystem (the immutable content-addressed store
and the mutable key-value store) and the version control subsystem (revisions, branches, staging,
sync, push, diff, merge, query) built on top. The CLI, language bindings, and the server are thin
layers over the same API; nothing is reachable only through a particular CLI flag or binding.

**Multi-tenant safe.** Multiple repositories can share a single backend deployment without
cross-tenant content leakage, even when tenants know or can guess each other's content hashes.

**Performance.** Performance is a first-class priority, not an afterthought. Scale is only
meaningful if the system stays performant at the scale it is being run at: cold-cache latency,
hot-path throughput, and memory footprint are design constraints from day one. Operations on a
multi-million-file repository must stay interactive; operations on a multi-gigabyte file must stream
rather than materialize; data structures are chosen for cache locality, zero-copy deserialization,
and constant-time indexing in preference to flexible-but-slow alternatives. The performance
discipline runs through the document rather than living in any one section: zero-copy node layout
in §10, fragment-time-not-stage-time hashing in §15, and stateless reads with edge tiering in §21.

**Backend-scalable.** The server tier scales horizontally. Hot/warm/cold storage tiering, edge
caching, and read replicas are deployment decisions, not invasive system rewrites.

**Lifecycle-complete.** Content can be added, deduplicated, *and* removed. Removal does not require
reference counting or rewriting Merkle history.

**Replaceable backends.** Storage and transport sit behind thin, documented interfaces. A
third-party storage backend or transport is a question of implementing those interfaces, not forking
the codebase.

**Open and free.** Released under the MIT license. The canonical implementation, protocols, formats,
and specifications are equally open and publicly versioned; there are no licensing constraints on
use, modification, embedding, or independent reimplementation.

#### 3.2 Explicit non-goals

**Peer-to-peer decentralization.** Lore is centralized by design. Two clients communicate through
the remote, not directly.

**Adversarial-server threat model.** Clients hash-validate every fragment they receive, so content
tampering is detectable end-to-end regardless of who served the bytes. The trust clients place in
the server is narrower - that it correctly reports the latest revision of a branch, the name-to-ID
mappings, and the access decisions for the session - and that trust is bounded by the server's
authentication and authorization model. Lore does not aim to defend against a server that is itself
adversarial about its own pointers or permissions.

### 4. Conceptual Model

Before the architecture, the mental models the rest of the document leans on: how Lore splits into
storage and version control subsystems, the two stores inside the storage subsystem, content
addressing, the revision graph, the Merkle tree, and where mutability lives.

#### 4.1 Two subsystems: storage and version control

Lore is structured as two independently usable subsystems.

The *storage subsystem* provides content-addressed immutable storage and a small mutable key-value
store, with its own public API. Operations include adding fragments, looking up fragments by
address, advancing mutable pointers atomically, and listing entries; the storage subsystem knows
nothing about revisions, branches, or files.

The *version control subsystem* (revisions, branches, merges, staging, sync, push, diff, query) is
implemented on top of the storage subsystem and exposes its own public API.

The separation is not an implementation detail. An application that needs only the storage
primitives - to deduplicate large data sets, to address content cryptographically, or to ship binary
blobs through a multi-tenant service - can use Lore as a storage system and ignore version control
entirely. An application that wants version control gets it built on the same primitives, with the
same scaling, performance, and isolation properties.

#### 4.2 The two stores

Within the storage subsystem, Lore is built on two storage primitives that handle different kinds of
state.

The *immutable store* holds content. Every byte that is ever written - file payloads, fragmented
file pieces, serialized revision states, serialized tree nodes, metadata blobs - goes here. Entries
are addressed by the BLAKE3 hash of their bytes; the hash is the unique signature of the data, and
any change to the data produces a different hash. The mapping between data and address is
one-to-one. Entries can be added or obliterated, never modified in place: there is no "modify"
operation, because changing the bytes would just produce a different entry at a different address.

The *mutable store* holds pointers and names. The latest revision of each branch, the mapping from a
branch name to its identifier, the catalog of repositories in a deployment: anything that needs to
be updated rather than appended to. This store is small in volume, but it is where consistency,
serialization, and contention live.

The split is not a deployment detail; it is the substrate. Most questions in the rest of the
document reduce to which store holds the data and how the two interact.

#### 4.3 Content addressing as the substrate

Content addressing propagates through the rest of the design. Deduplication is automatic: two
clients producing the same bytes produce the same address and store one copy. Integrity is
automatic: any bytes returned can be re-hashed and verified against its address. Transfer is
idempotent: re-sending the same fragment is harmless. And immutability is forced: changing the bytes
changes the address, and the old address still resolves to the old bytes.

#### 4.4 The revision graph

A *revision* is a frozen snapshot of the entire repository tree, identified by the hash of its
serialized state. Each revision references its parent or parents - one for an ordinary revision, two
for a merge revision. Revisions form a directed acyclic graph in which every edge is a cryptographic
link.

A *branch* is a named, mutable pointer to a revision in this graph - the branch's *latest* revision,
Lore's equivalent of Git's HEAD. Branches do not own revisions; a branch is just a name pointing at
one revision in the graph. Operations such as rebase, cherry-pick, and merge are operations over the
graph that produce new revisions whose parents and contents follow from the operation semantics.

#### 4.5 The repository as a Merkle tree

A revision's tree of files and directories is itself a Merkle tree. Each file node's hash derives
from the file's content (directly, or via a fragment list when the file is large enough to be
split). Each directory node's hash derives from the sorted hash list of its children. The root hash
uniquely identifies the entire repository state - every byte of every file in the revision is
reachable and verifiable from a single 32-byte root hash.

This is the structure that makes structural deduplication free. Two revisions that differ in one
file share every node not on the path from the root to that file, and "share" here means the same
hash, the same storage entry, no copy. Storage cost grows with what changed, not with repository
size.

#### 4.6 Where mutability lives and why it has to live somewhere

A purely content-addressed system has no notion of "current." A revision is just a hash; without
something pointing at it, the revision is unreachable the moment it is created. Branch latest
pointers, name lookups, deployment catalogs - these are inherently mutable, and no amount of
content addressing removes the need for them.

Lore's response is to keep the mutable surface as small as possible and concentrate it in the
mutable store. Most of the system never writes to that store; the parts that do are localized, well
understood, and the exclusive home of the system's serialization, contention, and consistency
concerns. Everything else can be replicated, cached, and verified independently.

### 5. Architecture Overview

§4 established the concepts; this section translates them into running software. The same library
code runs on both sides of the network and in both client and server roles; what differs is which
layers are exposed and where the wire boundary sits. This section walks the layered stack, the
client/server boundary, and the composition primitives - links and layers - that let multiple
repositories function as one.

#### 5.1 The layered view

Lore has two parallel stacks - one on each side of the network - that share the same internal
subsystems and communicate over the storage protocol. The server is not a layer beneath the client;
it is a peer that implements the server end of the remote services for both subsystems.

```
            Client side                             Server side
+---------------------------------+   +---------------------------------+
|  Tools (CLI, IDE)               |   |                                 |
+---------------------------------+   |  Server services                |
|  Public API                     |   |  (protocol termination, auth,   |
|  (C header + language bindings) |   |   policy, admin, replication)   |
+---------------------------------+   +---------------------------------+
|  Version control subsystem      |   |  Version control subsystem      |
+---------------------------------+   +---------------------------------+
|  Storage subsystem              |   |  Storage subsystem              |
+---------------------------------+   +---------------------------------+
            ^                                       ^
            |   standardized protocol over network  |
            +---------------------------------------+
```

The two subsystems - storage and version control - are the same on both sides. On the client side,
the *public API* is Lore's primary external artifact: the C header is the canonical interface, and
language bindings (Rust, Python, JavaScript, etc.) are equivalent surfaces over the same operations
- two sides of the same coin. The CLI, IDE integrations, and any other application using Lore from
outside are tools built on top of that API.

The server side is structurally similar but the public API is not in its path. *Server services* are
internal to Lore - they terminate the network protocol, authenticate sessions, enforce policy, and
handle operational concerns like replication and admin - and they call into the subsystems through
their internal interfaces directly, without going through the public API.

The boundary between any two layers is an interface contract, not a private implementation detail.
Server services dispatch into either subsystem depending on the request.

Underneath each storage subsystem is a durable backing - bytes have to live somewhere - but this is
a configuration choice, not an architectural layer. The client side typically uses local files plus
a fragment cache; the server side uses whichever durable storage the deployment provides. The
storage subsystem does not constrain the choice.

#### 5.2 The immutable data store

The immutable store is a content-addressed map from `(partition, address)` to bytes. Its public API
is a small set of operations: write a fragment, read a fragment by address, query whether a fragment
exists in a given partition, copy a fragment between partitions, obliterate. Fragments are
independent of one another; reading fragment X never requires reading or even knowing about
fragment Y. The store's scaling shape is therefore highly parallel: read paths are stateless,
lookups are sharded by hash, and a fragment can live in any backing storage that supports hash
keyed blob storage.

#### 5.3 The mutable key-value store

The mutable store holds the small set of pointers and names that cannot be content-addressed -
latest pointers, name-to-ID mappings, repository catalog entries. Its API is intentionally narrow:
`load`, `store`, `cas`, `list`. The store is small in volume but large in consequence: it is the
only place in the architecture where two clients pushing to the same branch must serialize against
each other. Backend implementations differ mostly in how they implement the `cas` (compare-and-swap)
primitive (a conditional swap on a single key, transactional row update, etc.); the choice of
backend is largely a question of how the deployment wants to handle consistency and failover at
this point.

#### 5.4 The version control subsystem

The version control subsystem implements revisions, branches, merges, staging, and sync as
compositions of storage operations. To commit, the subsystem writes a sequence of fragments to the
immutable store and then issues a single conditional put on the latest pointer in the mutable
store. To diff, it walks the two trees and fetches only the fragments along the differing paths. To
switch an instance, it walks the difference between the current materialized tree and the target
tree and fetches the fragments needed to make the difference visible on disk. The pattern recurs:
version control operations reduce to compositions of storage primitives.

#### 5.5 Client and server: where the wire sits

A client speaks to the remote through the storage protocol; the remote terminates the protocol and
processes the request against its own storage and version control stack.

A client holds working state on disk, staged intent, a fragment cache, and a small local mutable
store containing the branches it has materialized. Most everyday operations - diffing, switching to
a cached revision, listing locally known branches - happen without contacting a remote.

The remote holds the durable canonical state for the partitions it serves, authenticates and
authorizes sessions, validates push payloads end-to-end, and atomically advances latest pointers. A
client speaks to whichever endpoint of the remote is nearest, and the remote collaborates internally
to satisfy reads from the closest tier with fall-through to the durable backend.

#### 5.6 Sub-repository links and per-directory access control

A *link* is a reference from one repository to a specific revision of another, mounted at a path in
the parent's tree. The link is recorded in the parent's revision and travels with it: every clone of
the parent sees the same links. Architecturally, a link is just a tree node with a flag and a target
address; functionally, traversing into a link transparently switches to the linked repository's
storage and version control state.

The architectural consequence that matters most is access control. A *partition* is Lore's access
boundary; inside a single partition, Lore deliberately does not have per-path ACLs - every byte in a
partition is reachable to a session that has access to the partition. The per-directory access
control model emerges from links instead: each repository is one partition; each link mounts a
separate repository; each link is therefore a boundary at which access control changes. Any
directory that needs its own access policy is elevated to its own repository and linked into the
parent at the desired path.

A user with access to the parent but not to the linked repository sees the link node but cannot
descend into it. A user with access to both sees a single seamless tree. The two repositories remain
independently versioned, independently deduplicated, and independently governed; the link is the
only thing asserting that they are presented together at a given path.

The same mechanism handles vendoring, multi-team ownership boundaries inside a monorepo, and tightly
versioned dependency graphs. The recurring pattern is composition at the version control layer with
isolation at the storage and access control layer.

#### 5.7 Layers: local overlays

A *layer* is an overlay of a subset of one repository's content into another at a path, applied
locally rather than stored in the parent's revision. A layer is configured on a workstation and does
not travel with a clone; two machines on the same revision can have different layer configurations.

Architecturally, layers and links solve overlapping problems with different semantics. A link is a
versioned dependency every consumer of the parent sees; a layer is a local decoration of the
instance that attaches content without changing what gets committed. The choice between them is a
question of where a piece of composition belongs - in the revision (link) or on the workstation
(layer).

### 6. API-First Design

The API is Lore's primary artifact. This section unpacks what that commitment means in practice and
the discipline it imposes.

#### 6.1 The API is the primary artifact

The C header is the canonical interface to Lore. It exposes both subsystems - storage and version
control - through a flat C ABI. Language bindings (Rust, Python, JavaScript, etc) wrap the same
operations, with the same semantics, in the idiom of each language. The C header and the bindings
are two sides of the same coin: bindings do not extend the API, and the C header does not constrain
the bindings beyond the operations they all share.

Tools sit on top. The CLI is one application that uses the API; an IDE integration is another; a
remote service is another. None of them reaches into the subsystems behind the API.

#### 6.2 What the API exposes

Both subsystems are first-class.

The storage subsystem exposes content-addressing operations: read a fragment by address, write a
fragment, query whether a fragment exists in a partition, obliterate. It also exposes the mutable
store: `load`, `store`, `cas`, `list`.

The version control subsystem exposes the operations that turn that storage into version control:
open a repository, stage and commit changes, create branches, merge, rebase, cherry-pick, diff,
query history, sync with a remote, manage links. None of these is reachable only through the CLI;
every one is a function call on the C header.

#### 6.3 The storage subsystem as a standalone API

The storage subsystem is a first-class public API, usable with no repository, no branch, and no
revision involved. An application opens a store handle and operates on content directly:

- *Open and close.* A store handle is opened in one of two modes: *disk-backed*, against a
  local path whose packfiles persist across runs, or *in-memory*, a fresh ephemeral store that
  lives only as long as the handle - useful as a transient deduplicating cache, a scratch store
  for a pipeline stage, or a test fixture. Either mode can carry an optional remote endpoint for
  content that must be fetched from or pushed to a peer storage service. The handle is the unit of
  access; nothing about revisions or working trees is required to obtain one.
- *Content operations.* Through the handle, an application writes and reads content-addressed
  fragments, copies a fragment from one partition to another, uploads not-yet-durable local
  content to the remote, and obliterates content.
- *File ingest and extract.* The same handle reads a file directly into content-addressed
  fragments - chunking and hashing it through the storage subsystem's own machinery (§8) - and
  writes a stored payload straight back to a filesystem path, without the caller assembling bytes
  in memory or implementing chunking itself. This is the bridge that lets storage back an asset
  or build pipeline directly.
- *Metadata probe.* An application can fetch a fragment's metadata - its size, flags, and
  compression - without paying for the payload bytes, to decide whether content is present or
  worth fetching before committing to the transfer.

Operations are batched: a single call carries many items and reports a per-item outcome, so a
caller can submit thousands of fragments at once and learn exactly which succeeded and which
failed without serializing on a single result.

Two properties make this standalone surface worth exposing.

It is *multi-tenant-safe storage with full deduplication, by construction*. Every operation is
scoped to a partition (§7.3), and the partition is the access boundary: a session bound to one
partition cannot read, write, copy into, or probe another, even when both hold identical bytes.
Underneath that boundary the store deduplicates aggressively - identical content occupies a
single physical slot regardless of how many partitions reference it - but deduplication never
crosses the access boundary at the API: possessing a hash is never sufficient to reach content
in a partition the session is not entitled to (§17.5, §17.6). Storage cost is per-byte; access
is per-partition; the two are decoupled without compromising either.

It is *completely independent of version control*. The storage API knows nothing about revisions,
trees, branches, staging, or merges - it deals only in partitions, addresses, fragments, and
bytes. An application that needs content-addressed, deduplicated, access-controlled blob storage
- a build cache, an asset pipeline, a backup target, a multi-tenant artifact service - can use
Lore purely as a storage system and never touch the version control subsystem. The version
control subsystem is then one such consumer of the storage API among many, not a privileged
layer with private access to it.

#### 6.4 Specifications behind the API

For the API to function as a contract, the artifacts that pass through it must be specified
independently of the canonical implementation. Lore publishes:

- The on-disk and on-wire data formats - revision states, Merkle tree serialization, node and
  node-block layouts, name tables, fragment metadata, fragment references, mutable-store keys.
- The wire protocols end-to-end - framing, opcodes, payload layouts, error codes, session model.
- The storage backend interface - the operations a third-party backend must implement to slot in
  beneath the storage subsystem.
- The authentication and authorization contract - JWT resource scoping, partition derivation from
  session.

These specifications are versioned. Every fragment carries its format version; every revision state
carries its format version; every protocol header carries its protocol version. Compatibility
between two implementations is a question of which versions each supports, managed at the spec level
- there is no "build N matches build N" hidden contract.

#### 6.5 The discipline this imposes

Treating the API and its specifications as the primary artifact has direct consequences:

- All-binary, little-endian, fixed-size structures are preferred wherever reasonable. Small,
  unambiguous specifications are easier to implement correctly and to validate against than clever
  variable-length encodings.
- Bugs in the canonical implementation are bugs to fix, not behavior to preserve. A second
  implementation that does what the spec says rather than what the canonical implementation happens
  to do is correct, not deviant.
- Behavior absent from the spec is undefined. Conformant implementations do not have to match each
  other on undefined behavior, and downstream code cannot rely on it.

#### 6.6 What this enables

A third party can reimplement the client, the server, or any storage backend without
reverse-engineering anyone's binaries; the spec is the contract. Tooling can be built against Lore
without going through the CLI as an awkward intermediary; everything the CLI does, a tool can do the
same way. Bytes written by one implementation today remain readable by a different implementation
tomorrow, because the format is the specification, not whatever the writer chose to do.

The deeper consequence is that the boundary between "what Lore is" and "what's built on Lore" stays
clean. An IDE plugin, a code review system, a CI pipeline, or an internal tool is a peer of the CLI,
not a second-class consumer. There is no privileged path into the system.

#### 6.7 Semantic versioning and backwards compatibility

Lore's stability commitment for the wire protocols and serialized data formats is *semantic
versioning* with strict backwards compatibility. A version is a tuple `MAJOR.MINOR.PATCH`: a bump to
MAJOR signals an incompatible breaking change, a bump to MINOR signals a backwards-compatible
addition, and a bump to PATCH signals a backwards-compatible fix or change that does not affect the
ABI. Once a format reaches 1.0 the obligation is strict: no breaking change ships without a MAJOR
bump, and a 1.x reader must accept everything any earlier 1.x writer produced.

Lore is not yet at 1.0. Until it gets there, pre-1.0 semantic-versioning conventions apply - formats
and protocols may still change in incompatible ways between minor revisions, and clients and servers
should run compatible builds. Reaching 1.0, and then holding that line for as long as possible, is
an explicit project goal: the API-first commitment is only meaningful when consumers can bind
against a version and have it keep working.

One guarantee already holds, even pre-1.0: a newer version of the library can always read what an
older version has written. Data committed to a Lore repository today remains readable by every
future Lore release - the format may grow with new fields, opcodes, or compression options, but old
payloads stay intelligible to new code. Data written into Lore does not age out.

### 7. Content-Addressed Immutable Storage

The immutable store is the foundation that everything else in Lore is built on. This section
explains how content addressing is implemented in detail: the address structure, the role of
partitions and context, and the on-disk vs on-wire format split.

#### 7.1 How content addressing works

A fragment in the immutable store is a payload of bytes together with its BLAKE3 hash. Writers
hash the payload and store it under that hash; readers supply the hash and receive the payload,
verified against it. The address is derived from the bytes, not assigned by the writer; there is
no other way to refer to a fragment. Everything else in this section is layered around that bare
mechanic.

#### 7.2 BLAKE3 as the address function

Lore uses BLAKE3, a 256-bit cryptographic hash, as its address function. The choice trades off
three properties: cryptographic strength (collisions and second preimages must be infeasible),
throughput on commodity CPUs in pure software (the hot path for both writes and reads), and
parallelism over multi-core machines and SIMD lanes (because hashing a multi-gigabyte file
serially is unacceptable). BLAKE3 satisfies all three: faster than SHA-256 on long inputs,
competitive with non-cryptographic hashes on short ones, and structured as a Merkle tree
internally so that parallel hashing comes naturally.

The 256-bit output is large enough that collision risk is negligible across any plausible
repository size: a workload would need on the order of 2^128 fragments before the birthday bound
becomes a concern, which is many orders of magnitude beyond any conceivable repository.

#### 7.3 Partitions: access boundaries, not content addresses

A *partition* is a 16-byte opaque identifier that scopes content in the immutable store. From the
API, the partition is the access boundary: a session bound to partition A cannot look up
fragments registered to partition B, even if both partitions have fragments with the same hash.
Authorization on a session is bound to a partition.

Underneath the API, the storage subsystem is free to deduplicate identical bytes across
partitions - storage cost is per-byte, not per-partition - but this never leaks at the API. Two
clients with hashes for the same bytes in different partitions cannot use that knowledge to reach
each other's content; cross-partition reuse on upload is allowed, but only on proof of possessing
the bytes, never on hash knowledge alone.

The hash answers *what* a fragment is; the partition answers *who can access it*. The two are
deliberately separate. Deduplication and isolation are not in tension because they are about
different things.

#### 7.4 Context: identity that travels with bytes

A *context* is a 16-byte opaque tag carried alongside a content hash. Where the hash answers
"what bytes are these?", the context answers "what entity is this fragment part of?". Context is
not an access boundary - that role belongs to the partition - and it does not change what is
stored; the same payload bytes are still deduplicated under the same hash. Context is metadata
that travels with the address.

The motivating use is file identity. When a file is committed, then renamed or moved, then
modified, the file's bytes change but its identity does not. Lore tracks this with a stable
per-file ID assigned at file creation; that ID is the context for every fragment that belongs to
the file. Version-control bookkeeping uses the (hash, context) pair to track moves, copies, and
obliteration scopes without breaking the underlying dedup.

The context is generic: any 16-byte tag chosen by the caller of the storage subsystem. The
version control subsystem uses it primarily for file identity, but the storage subsystem itself
doesn't care what it means.

#### 7.5 The address: hash plus context

A complete *address* in the immutable store is the pair (hash, context) - 32 bytes for the
BLAKE3 hash, 16 bytes for the context, 48 bytes total. This is the unit by which fragments are
stored, looked up, transferred, and obliterated.

Two fragments with the same address are the same fragment. Two fragments with the same hash but
different contexts are distinct: they share a payload (deduplicated under the hood) but track
different identities, with different version-control histories and different obliteration
scopes. This is what lets version control track file identity across moves and copies, and
obliterate one file's content without disturbing another's, even when the two share bytes.

#### 7.6 Local packfiles vs wire-format fragments

The on-disk format and the on-wire format are not the same shape, by design.

On disk, fragments are stored in *packfiles* - large append-only files, each holding many
fragments back-to-back, with an index from address to offset. Packfiles amortize per-file syscall
and metadata costs across many fragments, which matters because a single revision can reference
millions of small fragments. The index is mmappable and supports random lookup in constant time.

On the wire, fragments travel individually. Each one carries a small header with its size, flags,
and compression marker, followed by its payload. There is no notion of a packfile in the
protocol.

The split exists because the two contexts have different optimization goals. On disk, the goal
is locality and amortized I/O: many fragments per file, mmappable indexes, append-only writes.
On the wire, the goal is independence, parallelism, and resumability. Independent fragments can
be transferred in any order, in parallel across multiple connections; the server handles them out
of order and returns responses as soon as each fragment is ready, without serializing on a
packfile boundary or maintaining ordering state. A transfer that fails halfway through leaves the
already-delivered fragments intact and lets the client resume by transferring only the remaining
ones. Treating disk and wire as one format would either force the disk to be fragment-at-a-time
(poor I/O behavior) or the wire to be packfile-at-a-time (no per-fragment parallelism, no
fragment-level dedup query before transfer, no resumable streaming).

The on-disk format is one possible format - implementations are free to choose another. The wire
format is what the protocol specifies, and is what conformant implementations must agree on.

### 8. Chunking and Fragmentation

The immutable store stores fragments by hash, but a file can be much larger than a single
fragment. This section explains how Lore splits large files into chunks, why two strategies are
needed, how the resulting fragment lists themselves are kept manageable, and how compression
fits in without disturbing the addressing model.

#### 8.1 Why chunking

Without chunking, content addressing operates at file granularity. Two files that share a
gigabyte of identical content but differ in one kilobyte produce different file hashes and store
the entire content twice. A single edit in the middle of a multi-gigabyte file would invalidate
the file's address and force a full re-upload. And random access into a file would mean fetching
the whole file before reading any byte of it.

Chunking solves all three. A file is split into a sequence of smaller fragments, each addressed
independently; the file's identity becomes the hash of the *list* of fragment addresses, not the
file's bytes. A small edit changes only the affected fragment(s) plus the list. Identical regions
across files dedupe. A byte range can be read by fetching only the fragments that cover it.

Chunking introduces a question, though: where do the boundaries go? Different strategies answer
it differently and have different cost profiles.

#### 8.2 Two chunking strategies

Lore supports two chunking strategies.

*Content-defined chunking* via FastCDC. A rolling hash slides over the file and a chunk boundary
is placed wherever the hash matches a magic pattern, subject to minimum and maximum chunk sizes.
Boundaries are determined by content, not offset: inserting bytes at the beginning of a file
shifts subsequent boundaries by the same amount, so a small edit in a large file produces chunks
identical to the unmodified file except in the immediate neighborhood of the change.

*Fixed-size chunking*. Boundaries are placed at fixed offsets regardless of content. Splitting
is trivial (no rolling hash, no scanning), and the address of any byte range is computable from
the byte offset alone without reading the file.

Each strategy is the right answer for some content and the wrong answer for others. In the
canonical implementation, FastCDC targets a 64 KiB average chunk with a 32 KiB floor and a
256 KiB ceiling; fixed-size chunking can be configured to any size up to that same 256 KiB
ceiling. The ceiling is the protocol-level fragment-size threshold (§18.3) - any payload that
exceeds it is itself fragmented through the recursive scheme of §8.4.

#### 8.3 Tradeoffs and per-content-type policy

Content-defined chunking is the right answer when sparse writes inside large files are common
and dedup across edits is valuable. Code repositories where occasional large checked-in artifacts
get edited; large data files that accumulate appends; binary formats that are frequently modified
in place: all of these benefit from CDC. The cost is that chunk boundaries depend on content, so
finding them requires scanning the bytes - chunking a fresh file is O(file size), and dedup
requires re-running the chunker on bytes that may already exist somewhere in the store.

CDC carries a subtler cost. Truly sparse writes require *reusing the previous chunk boundaries*
where the content has not changed; otherwise the rolling hash, run from scratch over a modified
file, can match its magic pattern at different points than before, producing different boundaries
in regions that did not actually change. The result is a cascade of new chunks across unchanged
content, defeating the dedup CDC was supposed to deliver. Implementations therefore use a
temporal-coherence strategy: when re-chunking, reuse the prior boundaries where the bytes are
unchanged, falling back to fresh CDC only where the content has shifted.

Reusing prior boundaries preserves dedup but costs canonicality. The address of a CDC-chunked
file depends on its chunking history, not on the bytes alone: re-chunking the same file from
scratch can produce a different top-level hash than incremental re-chunking that reused prior
boundaries. The same content can therefore have multiple legitimate addresses. Formally, with
CDC the property `address(A) != address(B) => content(A) != content(B)` ("different addresses
imply different content") does not hold.

Fixed-size chunking is the right answer when canonical addressing matters more than dedup
robustness, or when the property above must hold. Boundaries are determined by offset alone, so
the same content always produces the same chunks and therefore the same top-level hash,
regardless of chunking history. The address is canonical - one piece of content has exactly one
address - and `address(A) != address(B) => content(A) != content(B)` holds unconditionally. A
chunk at offset 12 GiB has a deterministic address that any party can compute by reading just
that chunk's bytes. The cost is that dedup is preserved only for aligned, identical regions;
an insertion at the beginning of a file shifts every subsequent boundary, and no chunk after
the insertion deduplicates with the original.

Lore offers both strategies; the choice for any given content is made by the application calling
the storage subsystem, not by Lore itself. An application can map content types to strategies in
whatever way fits its workload - CDC for sources where dedup across edits matters, fixed-size
where the canonical-address property is required, or any other policy. Lore does not impose a
built-in mapping. The storage subsystem doesn't care which strategy produced a chunk; both
produce ordinary content-addressed fragments.

#### 8.4 Recursive fragmentation

A large file produces many chunks, and the list of fragment references itself can exceed the
fragment-size threshold; for a multi-terabyte file, the list of references may be hundreds of
megabytes.

Lore's response is to recursively fragment the list. If a fragment list exceeds the threshold,
the list is itself chunked and stored as a fragment with a flag marking it as a list, and the
file is now described by a tree of fragment lists rather than a flat list. Each level of the
tree is independently addressed, deduplicated, and lazily fetchable. The recursion bottoms out
when a fragment list fits in a single fragment.

The tree is mostly invisible at the API level. A reader asks for a byte range and gets the
bytes; the storage subsystem walks the tree, fetching only the fragments needed to cover the
range. Advanced use of the storage API can read and write the raw fragment list data if need be.

#### 8.5 Fragment references and sparse reads

A *fragment reference* in a fragment list records two things: the hash of the referenced fragment
and the byte offset it represents in the reassembled content. The offset is what makes sparse
reads possible.

Fragment references in a list are kept strictly ordered by offset, so the list is indexable: a
binary search can find the fragment covering any given offset in O(log n) time without
materializing the file (the current implementation is linear but can be improved to take
advantage of this). Reading a byte range fetches only the fragments that overlap the range -
typically a small fraction of the file - and those fragments can be fetched in parallel and
processed out of order, since each is independent of the others. Sparse reads scale with the
requested range, not with the file size.

#### 8.6 Compression as an orthogonal concern

Compression is per-fragment and orthogonal to addressing. Lore supports Zstd today, and the
codec list is open-ended; fragment metadata records which compression (if any) was applied to
the stored payload. The address is the hash of the *uncompressed* fragment content, so changing
or replacing the compression algorithm does not change the address. Dedup is preserved across
compression choices.

This separation is deliberate. Compression is a storage and transfer optimization; addressing is
identity. Conflating them would mean that two clients compressing the same content differently
would produce different addresses, breaking dedup. Keeping them separate means a client can
choose the compression that suits its CPU and bandwidth budget without affecting the
content-addressed model.

### 9. The Mutable Store

The immutable store handles everything that's content-addressable; the mutable store handles
everything that isn't. This section unpacks the latter: what lives there and why, how the local
and remote stores diverge and reconverge, why branch identity is decoupled from branch name, and
how a single compare-and-swap primitive (`cas`, §9.4) gives Lore its atomic state transitions.

#### 9.1 What lives in the mutable store and why

A content-addressed store provides "fetch the bytes for hash H." It does not provide "fetch the
current state of branch main", because the answer changes over time, and a hash that changes is
no longer a hash of anything. Talking about *current*, *latest*, or *named* requires a layer
that maps stable identifiers to current values. That is what the mutable store is for.

The contents fall into a few categories.

For branches:

- *Per branch ID*: a *latest pointer* (most recent revision hash) and a *metadata pointer*
  (referring to a content-addressed metadata blob in the immutable store).
- *Per branch name*: a *name-to-ID mapping* resolving each branch name (like `main`) to its
  opaque branch ID.

For repositories:

- *Per repository ID*: a *metadata pointer* (referring to a content-addressed metadata blob in
  the immutable store).
- *Per repository name*: a *name-to-ID mapping* resolving repository names to their opaque IDs.

The pattern across every entry is the same: a small key that names a piece of mutable state, a
value that is either an inline pointer (a hash or an address) or another opaque identifier. The
mutable store does not hold the metadata *bodies*; those are content-addressed and live in the
immutable store. The mutable store holds only *which* immutable blob is currently in effect,
plus the lookups needed to find blobs by name or ID.

Like the immutable store, the mutable store is partition-scoped. Every mutable key lives in
exactly one partition; two keys with the same name in different partitions coexist as distinct
entries. A session bound to partition A cannot read or write keys in partition B, and the
conditional-put primitive is enforced per partition. Partitions are the access boundary for the
entire storage subsystem, not just the content-addressed half.

What's missing from this list is instructive. There is no enumerable list of revisions or
fragments; revisions live in the immutable store and fragments are addressed by hash, not
listed. There is no per-revision metadata that varies over time; all metadata about a revision
is part of the revision's serialized state, which is itself immutable. The mutable store
contains *only* what cannot be expressed by content addressing.

#### 9.2 Local and remote, both canonical

The mutable store is not a single global resource. Each client has its own mutable store, and
each remote has exactly one. The remote's mutable store is the single canonical representation
for the deployment: even though a remote may consist of many server processes and cache tiers,
they all share one logical mutable store. Within a deployment, branch state does not diverge
between instances.

What does diverge is the client's mutable store and the remote's, whenever the client is doing
local work that has not yet been synchronized; and two separate remotes (different deployments
mirroring or replicating each other), whenever they have not yet exchanged their latest state.

This divergence is what makes offline operation possible. A developer working without network
connectivity can still look up the latest revision of a local branch, commit new revisions
(advancing the local latest pointer), create or archive local branches, and diff against any
locally cached revision. None of these contact the remote. When the client reconnects and
syncs, the local and remote stores compare notes: a push proposes the client's local view as
the new remote view, conditional on the remote's current view matching what the client believed
it to be; a fetch updates the client's view from the remote.

The divergence is not a bug; it is what allows the system to be both centrally coordinated and
offline-capable. Within a deployment there is nothing to converge - the mutable store is single
and authoritative. Across the client/remote boundary or across deployments, convergence happens
at sync points, not continuously.

#### 9.3 Identity vs name

A repository and a branch in Lore each have two identifiers: a stable opaque *ID* (a UUIDv7)
and a human-readable *name*. The name-to-ID mapping is itself a mutable-store entry; the
per-ID entries (latest pointer, metadata pointer) hang off the ID, not the name.

The decoupling matters. A branch ID never changes for the lifetime of the branch; it is what
identifies the branch's history. A branch name is a label that can be archived (the
name-to-ID mapping is removed, but the branch metadata and latest pointer remain), restored
(the mapping is put back), or reused (a new branch can take the name of an archived one
without clobbering the archived branch's history). Tools that need to refer to a branch
unambiguously refer to it by ID; tools that need to present the branch to a human user
use the name and let the mutable store resolve it.

#### 9.4 Atomicity via compare-and-swap

The mutable store's API is narrow: `load`, `store`, `cas`, `list`. The `cas` operation
(compare-and-swap) is what gives Lore its atomic state transitions. A typical advance is "set
this branch's latest revision to H_new, but only if it currently equals H_old": if the
precondition holds, the swap succeeds; if a concurrent writer got there first, it fails, and
the caller must re-establish the precondition before retrying.

The same primitive applies to every mutable entry, not just latest pointers. Updating a
branch's metadata is the same shape: the writer constructs a new metadata blob, uploads it to
the immutable store (where it gets a new hash), and issues a `cas` on the branch's
metadata pointer to refer to the new blob's hash. Renaming a branch, advancing repository
metadata, restoring an archived name, registering a repository in the catalog - all reduce to
"write the new immutable blob (if any), then `cas` the relevant key in the mutable
store." The protocol exposes the `load`, `store`, and `cas` operations directly; clients and
servers coordinate every mutable-state change through the same contract.

This is the only true serialization point in the architecture. Two clients reading do not
contend; two clients writing fragments to the same partition do not contend. The contention is
exclusively at the `cas` on a mutable key, and only when both writers target the
same key.

Atomicity follows from this single primitive. A higher-level operation (a commit and push, for
instance) writes all its fragments to the immutable store first, then issues a single
`cas` to advance the latest pointer. A push interrupted before that put is not
visible to readers - the fragments are in the immutable store but no pointer points at them
yet, and the branch still appears at its prior latest revision. The advance is the only thing
that promotes a set of fragments to "the current state of the branch", and it either happens
fully or not at all.

### 10. Revisions and the Merkle Tree

This section gives the implementation behind the conceptual revision and tree model: the
320-byte revision state, the fixed-size node layout that makes the tree cheap to traverse, the
block sharing that makes structural dedup automatic, the chain integrity that makes history
tamper-evident, and the graph operations (branch, merge, rebase, cherry-pick, squash) that
produce new revisions from existing ones.

#### 10.1 Revision state as a 320-byte fragment

A revision is a 320-byte blob in the immutable store. Despite the name *revision state*, it does
not contain the tree, the metadata, or the file list directly; it contains hashes of those
things. The fields are:

- A magic number and a format version for self-identification.
- The revision number (an integer that increments by one along a branch's first-parent chain).
- Hash signatures of the parent revision states - one for an ordinary revision, two for a
  merge.
- The hash of the serialized Merkle tree (the full file and directory structure).
- The hash of the serialized metadata (revision commit message, author, timestamps, additional
  key-value metadata).
- The hash of the serialized link list (for repositories that mount linked repositories).
- A repository ID for the second parent in cross-repository merges.
- A few reserved fields for forward compatibility.

The size matters. 320 bytes is small enough to be freely cached, fetched in one round trip,
and validated without further fetches. The branch's *latest pointer* in the mutable store is
the hash of this 320-byte blob; that hash pinned to a 320-byte payload is the root of
everything else. Loading a revision is one read and consumes insignificant memory; loading the
tree, the metadata, or the links is on-demand from there.

#### 10.2 Fixed-size node and node-block layout

The Merkle tree is not stored as a recursive serialized structure. It is stored as a sequence
of *node blocks*, each holding a fixed number of fixed-size *node* records. A node is 96 bytes;
a node block holds 512 nodes plus a 128-byte block header, totaling 49280 bytes - small enough
to be a single fragment.

A node carries the data needed to describe one file or one directory entry: flags, file mode,
indexes into the tree (parent, child, sibling, all 32-bit), a name reference (offset and length
into the block's name table, plus a 64-bit hash of the lower-cased name for fast lookup), the
file size, and the content address (hash plus context, where context is the file ID for file
nodes).

Names are not stored inline in the node. Each block carries its own name-table fragment, and a
node's name is found by following its `name_offset` and `name_length` into that fragment; the
lower-case name hash on the node lets common lookups (case-insensitive name match) happen
without dereferencing the table at all. Keeping names out of the node lets node records stay
small and uniform regardless of file-name length.

Fixed-size everywhere is the design driver. A node block can be mmapped from disk straight into
memory and used without parsing or copying; node-to-node navigation is a 32-bit index lookup;
serialized size is identical to in-memory size. Zero-copy deserialization is the goal, and
every field width is set to make it work.

#### 10.3 Block sharing across revisions

Storing the Merkle tree as a list of node blocks gives Lore structural dedup for free. Two
revisions that differ in a few files share every node block that was not modified - the blocks
have the same hash, and the immutable store stores them once.

When a new revision is committed, the modified blocks are written as new fragments and the
unmodified blocks reuse the parent revision's hashes. The new revision's tree references the
union: mostly old block hashes plus a few new ones. A revision's storage cost is proportional to
the number of blocks it modified, not to the total size of the tree.

The same sharing applies to the auxiliary block streams the tree references alongside the
node blocks - per-file metadata blocks, the per-revision change log (the record of which paths the
revision changed) - all of which are stored as content-addressed fragments and shared across
revisions wherever unchanged.

#### 10.4 Chain integrity via parent hashes

A revision's state hash is computed over the full 320-byte revision state, including the parent
state signatures. Modifying any byte of a parent revision changes the parent's hash; the child's
recorded parent-signature still refers to the old hash, so the chain breaks visibly. Reaching
back through the chain, every revision is cryptographically linked to its parent(s), and the
entire history of a branch is verifiable from any current revision hash.

This is what makes content-addressed history tamper-evident. A server cannot quietly substitute
a different revision for an old one without changing every descendant's hash. A client that
fetches a revision and validates the chain has the same guarantee, regardless of who served the
fragments.

#### 10.5 Branching and merging

A *branch* is a name in the mutable store that resolves to a current latest-revision hash. There
is no separate "branch object" in the immutable store. Creating a branch from another branch's
revision is a mutable-store insertion: new branch ID, new name-to-ID mapping, latest pointer
initialized to the branch-point revision. No new revisions are written by the branch operation
itself.

A *merge* is a revision with two parents instead of one. Both parent state hashes are recorded
in the merge revision's 320-byte state; the merge revision's tree is the result of combining the
two parent trees, with conflicts resolved at commit time (automatic for non-conflicting changes,
manual otherwise). The graph remains a DAG because the merge revision is a fresh node, not an
edit to an existing one.

Two branches can share the same revision history up to some point and diverge from there, and
the mutable store can hold both names mapping to different latest revisions in the same shared
graph. The fragments in the shared portion of the graph are stored once.

#### 10.6 Rebase, cherry-pick, and squash

Rebase, cherry-pick, and squash produce new revisions from existing ones. In an immutable
model, no operation can rewrite history in place, so each "rewrite" is really a fresh sequence
of revisions plus a re-pointing of the branch's latest pointer.

- *Rebase*. Take a chain of revisions on branch A and replay each one as a new revision with a
  different parent (the latest of branch B). The replayed revisions are fresh entries in the
  immutable store with new hashes; the original chain still exists, but branch A's latest
  pointer moves to the head of the replayed chain.
- *Cherry-pick*. The same operation for a single revision (or a contiguous range). The
  cherry-picked revision is freshly written with a different parent; the original is unchanged.
- *Squash*. Collapse a chain of revisions into one revision whose tree is the final state and
  whose parent is the one before the chain. The squashed revision is a fresh entry; the
  collapsed chain still exists in the immutable store but is no longer reachable from any
  branch's latest pointer.

Three observations follow. First, the original revisions are not deleted by any of these
operations. They become unreachable from the branch's current latest pointer, but the fragments
are still in the immutable store and can still be referred to by hash. Second, the new
revisions inherit the same tree-block sharing as any normal revision - a rebase that replays
mostly unchanged trees on a new base rewrites very little storage; only the parent hashes
change. Third, the branch's latest-pointer advance is atomic: a long-running rebase produces all
its revisions in the immutable store first and flips the latest pointer in a single conditional
put at the end.

Not all of these operations are fully implemented in the canonical implementation today; the
data model supports each of them, but some are still on the roadmap.

### 11. Metadata

Metadata in Lore is a typed key-value store attached to entities in the system - files,
revisions, branches, and repositories. It is the primary extension point for both built-in
system data (revision commit messages, timestamps, branch categories) and arbitrary
application-defined data. This section explains where metadata lives, how immutable
revision-attached metadata differs from mutable branch- and repository-attached metadata, the
format, and how Lore divides the key space between built-in and application use.

#### 11.1 Where metadata lives: immutable vs mutable attachment

Metadata is stored as a content-addressed blob in the immutable store. What differs by entity
is *how* the entity references the blob, and therefore whether the metadata is part of the
committed revision history or is a mutable annotation outside of history.

- *Revision metadata*. Referenced by hash from the 320-byte revision state. Part of the
  committed revision: every clone of the revision sees the same metadata, and the metadata is
  hash-chained into the revision's state hash. It cannot be changed after commit; "revising" a
  revision's metadata would produce a different revision.
- *File metadata*. Stored in the file metadata block stream parallel to the node block stream.
  Part of the committed revision in the same way.
- *Branch metadata*. Referenced by a metadata pointer in the mutable store. Mutable: a write
  produces a new metadata blob in the immutable store and updates the pointer via the
  conditional-put primitive. Branch metadata can change over the lifetime of the branch
  without producing new revisions.
- *Repository metadata*. Same shape as branch metadata - a mutable pointer to a content-
  addressed blob.

The same content-addressed bytes back every metadata blob, regardless of attachment point. The
distinction is in the reference: an immutable reference (revision state, file metadata block)
makes the metadata part of the committed revision graph; a mutable reference (the mutable
store) makes it annotation that can evolve over time.

#### 11.2 Format and limits

A metadata blob is a serialized typed key-value array. The format is binary: a small header
(magic and version), followed by a sequence of items. Each item carries a key length, a value
length, a value type tag, and the key and value bytes. The value type is one of *address*,
*boolean*, *context*, *hash*, *numeric*, *string*, or arbitrary *binary*. There is no string
parsing, no escape characters, and no recursive structure - just typed fields concatenated.

The blob size is capped at 1 MiB. Applications that need to attach larger artifacts - binary
blobs, large structured documents - should store the artifact as its own immutable fragment
and reference it from a metadata key by hash or address. The metadata blob itself stays small,
cheap to load, and fully resident in memory while in use.

#### 11.3 Built-in keys and application keys

For each entity type, Lore reserves a set of *built-in* keys that the system itself reads or
writes. Examples include `message`, `timestamp`, `created-by`, `committed-by`,
`change-request`, and `cherry-picked-from` on a revision; `category`, `creator`, `created`,
and `protect` on a branch; `name`, `description`, `default-branch`, `creator`, and `created`
on a repository. Some built-in keys are read-only (assigned by the system at creation and
never modified); others can be set or updated by users, but the key itself is reserved.

The remaining key space is free for application use. Tools and integrations attach their own
keys for purposes the system itself does not know about - code review IDs, build provenance,
signing attestations, deployment tags. Lore does not interpret application keys; it stores
them and returns them as-is.

This makes metadata the natural extension surface for everything built on top of Lore. A code
review system attaches review state to revisions; a build system attaches provenance to files;
an operations team tags branches and repositories. The semantic model is the same in every
case: a typed key-value blob attached to the entity, read and written through the same API.

A concrete example threads several of these together. A revision is committed and the system
records its built-in keys (`message`, `timestamp`, `committed-by`). A code review service then
attaches its own application keys to the same revision - say, `review-id` (string) referring
back to a review record, `review-status` (string) tracking approval state, and `signed-off-by`
(string) listing approvers. A CI service later attaches `build-id` (string), `build-status`
(string), and `artifact-address` (address) pointing at a fragment containing the build output.
Each of these keys is on the same revision metadata blob; each is typed; none of them
interferes with the others; and none of them required Lore itself to know that code review
or CI exist. A tool wanting to render a per-revision dashboard reads the blob once, picks out
the keys it understands, and ignores the rest.

### 12. Sparseness and Partial Working Copies

Lore is built for repositories where the full tree is far too big to materialize on every
clone. Sparse instances and lazy data fetching are not power-user features bolted on the
side; they are the default operating mode of the system. This section explains how Lore
filters what gets to disk, what gets staged, what gets fetched, and how the local cache makes
the whole thing efficient.

#### 12.1 Why sparse is the default

A repository in Lore can hold millions of files and terabytes of content. No contributor ever
needs all of it on their machine: a frontend developer doesn't need iOS native code; a CI
runner building one component doesn't need the others; an artist editing textures doesn't need
the engine source. The instance should hold what the user is working on, not what every
team and tool in the repository is working on.

The architecture follows from this. Materialization cost should track the working subset;
fetch cost should track what hasn't already been seen; storage cost should track what's
actually been touched. Operations on a small part of the tree should run fast regardless of
how big the rest of the tree is. Sparseness is therefore the substrate, not a feature: the
materialization machinery is built so that doing less work is the natural case.

#### 12.2 View files: inbound filtering

A *view file* (`.lore/view`) is an inbound filter declaring which paths in the repository the
user wants on disk. Paths outside the view are not materialized when a revision is synced,
switched, or restored; they exist in the repository's tree, but no bytes for them are written
to the working directory.

The view is local to a client. It is not part of the committed revision and does not travel
with a clone. Two clients on the same revision can hold different views, materializing
different subsets of the same tree. A change to the view triggers the storage subsystem to
materialize (or de-materialize) paths to bring the instance into agreement.

Views shape what the local cache is asked to hold, what the remote is asked to send, and what
filesystem-level tools (search, indexing, build systems) see. They are useful well beyond the
obvious "I only work on this directory" case.

#### 12.3 Ignore files: outbound filtering

An *ignore file* (`.loreignore`) is an outbound filter declaring which paths the user does not
want included in commits. Paths matching ignore rules are not staged, not committed, and not
reported in status. Build artifacts, editor temporary files, machine-local configuration: all
typical inhabitants.

Ignore is purely about *new* content. A file that is already part of the committed revision
will keep flowing through inbound operations even if a later ignore rule would exclude it; the
ignore rule does not retroactively eject committed files. Removing such a file from the
revision is a separate explicit act.

#### 12.4 FilterMode: which filter applies when

Views and ignores answer different questions and apply at different times. Lore makes the
distinction explicit through *FilterMode*: every operation that touches paths declares which
of the two filters to consult.

The rule is simple: operations on *committed state* consult the view only; operations on
*local state* consult both.

- Operations that read or write the committed revision graph - syncing a revision, switching
  branches, computing a diff between revisions, restoring a file from a revision - filter
  through the view. They do not consult the ignore file: a file that was committed before an
  ignore rule was added must still flow through, otherwise the system would refuse to update
  perfectly legitimate committed paths.
- Operations on the working directory or staging area - staging, status, locking - filter
  through both. The view limits what the operation sees on disk; the ignore file then further
  excludes things the user has declared shouldn't be staged.

This split is what keeps `.loreignore` from breaking earlier history: a newly-added ignore
rule does not retroactively excise committed files, because committed-state operations consult
the view only.

#### 12.5 Lazy fetch

Sparse instances are only useful if fetching is lazy as well. Lore's storage subsystem
fetches fragments on demand: when a revision is loaded, only the parts of the tree the view
asks for are walked, and only the fragments backing those parts are pulled from the remote.
The rest of the revision exists in the immutable store on the remote (and possibly in edge
caches in front of it), but never touches the client's local store.

This pairs with the chunking model from §8. Reading a 4 MiB byte range from a multi-gigabyte
file does not require fetching the whole file; it requires fetching the fragments that
overlap that range. The same is true for tree traversal: walking one path to one file does not
require fetching every node block in the tree.

The remote's tier structure is invisible to the client. Hot fragments may be served from edge
caches close to the client; warm fragments from the deployment's primary storage; cold
fragments from a colder tier the deployment maintains. The client issues a single storage
request and the deployment satisfies it from whichever tier holds the bytes; tiering is a
deployment decision, not a client concern.

#### 12.6 The local cache

A client's local store doubles as a fragment cache. Fragments fetched on demand stay locally
once they are read, so subsequent reads of the same fragment do not contact the remote. The
cache is the instance's footprint plus whatever was loaded along the way to producing it.

The cache is sized by the user: an LRU policy evicts the least-recently-used fragments when
the configured budget is exceeded. A developer iterating on the same area of the repository
keeps a small working set hot; switching to a different area pulls in the new working set and
gradually evicts the old one. There is no system-imposed "right amount to cache" - the budget
is set per workstation according to the disk space the user is willing to spend.

Multiple repositories or clones on the same machine can share a single local store. Fragment
addresses are content-derived, so two clones that happen to share content also share the
cached bytes; the instances are separate, but the underlying fragment store is one.

### 13. Binary-First by Design

Lore treats every file as opaque bytes on the hot path. Text-aware behavior - line-oriented
diff, line-ending normalization, encoding inference, syntax-aware merge - exists in tools that
sit above the storage subsystem, not inside it. This section explains what binary-first means
concretely, how text features layer on top, what it implies for diff/merge/review tooling, and
how file-level locking handles content that cannot be merged at all.

#### 13.1 What "binary first" means concretely

Binary-first is a discipline about what the storage and transport paths *don't* do.

- No line-ending translation. Lore does not convert CRLF to LF or LF to CRLF, ever. The bytes
  on disk are the bytes that get hashed, the bytes that get stored, and the bytes that come
  back out. A file with mixed or unconventional line endings stays that way.
- No text encoding inference. Lore does not detect or rewrite encodings. UTF-8, UTF-16, plain
  ASCII, an arbitrary binary container that happens to contain readable text - all are
  treated identically, as byte sequences.
- No clean/smudge filters in the data path. There is no on-write transformation that rewrites
  file content during commit, and no on-read transformation that rewrites file content when
  the instance is materialized. Bytes go in unchanged; bytes come out unchanged.
- No 7-bit assumptions. Hashing, chunking, transfer, and storage all operate on full 8-bit
  bytes. There is no special handling of NUL, no encoding of "control" characters, no
  expectation that any byte value carries semantic meaning to the storage path.

Each of these is an absence, not a feature, and the sum of the absences is what makes
binary-first a coherent design. A repository that holds source code, configuration,
serialized data formats, large media assets, prebuilt artifacts, and arbitrary binary blobs
treats all of them with the same machinery - because the machinery treats none of them as
anything other than bytes.

#### 13.2 Text handling layered on top

Text-aware behavior is not absent from Lore; it is just not in the storage subsystem. Tools
that know about text operate on the bytes that come out of the storage subsystem and produce
their results. A diff tool reads the bytes of two files and renders a line-oriented diff; a
merge tool reads the bytes of three files (base, ours, theirs) and produces a merged result.

The arrangement is one-directional: text tools depend on storage, storage does not depend on
text tools. A new text-aware feature can be added on top without touching the storage
subsystem; a non-text consumer of the storage subsystem (a CDN, a build cache, a backup
system) does not have to know anything about text. Storage treats the world as bytes, and
text-as-such happens above.

#### 13.3 Implications for diff, merge, and review tooling

Binary-first means the *storage subsystem* knows nothing about text. The *version control
subsystem* above it ships built-in implementations of the common text-handling operations -
text diff, three-way text merge, line-oriented conflict resolution - so that ordinary
"two contributors edited the same file" workflows work out of the box. These implementations
are top-level features, not part of the storage layer, and applications are free to replace
them with their own.

- *Text diff and merge*. The version control subsystem ships a line-oriented text diff and
  a standard three-way merge implementation, used by default for text-shaped content.
  Applications that want different semantics - word-level diff, semantic merge,
  language-aware tooling - can plug in their own without touching anything below.
- *Binary diff and merge*. There is no universal answer. A texture-file diff is meaningful as
  an image, not as a hex dump; a 3D model merge requires a tool that understands the model's
  topology; a database snapshot diff is a different operation again. Lore makes the bytes
  available; specialized tools do the rest.
- *Review surfaces*. Code review systems built on top of Lore can render each file with the
  viewer appropriate to its type - source as a syntax-highlighted line diff, an image as
  side-by-side rendering, a binary as a structured viewer. The renderer chooses per file;
  Lore neither helps nor hinders.

The principle: storage stores raw bytes; version control provides default semantics for the
common cases; applications are free to replace those defaults when they need different
behavior.

#### 13.4 Locks for unmergeable content

Some files cannot be merged in any meaningful sense. A binary world-state file in a game
engine, an image authoring document, a serialized scene - any format where two parallel edits
produce a result no automated process can reconcile, and where a manual three-way merge would
mean opening the file in a tool and re-doing the work. For these files, optimistic concurrency
(commit, discover the conflict, merge) is the wrong model.

Lore offers explicit file-level locking for this case. Three operations cover it:

- *Acquire*. A user takes a lock on a file before editing it. The lock is recorded server-side
  and visible to other clients.
- *Release*. The lock is released when the user is done with the file, freeing it for others
  to acquire.
- *Query*. Anyone can ask whether a file is locked and, if so, by whom.

A locked file cannot be modified by anyone other than the lock holder; an attempt by another
client to push a change to a locked file is rejected by the server before the change becomes
visible. The signal is "this file is being edited; do not start your own edit until the lock
is released," and the cost of ignoring it is a refused push, not a corrupted history.

Locking does not replace the underlying optimistic-concurrency model that backs everything
else; it complements it. Files that can be merged stay on the optimistic path; files that
cannot are locked when in use, and the rest of the repository carries on around them.

### 14. Centralized with Offline Capability

Lore is centralized in *role* but not in *availability requirements on every operation*. The
remote is the source of truth for what is canonical; the client holds enough state locally to
operate without contacting it for ordinary editing work. This section unpacks why centralized,
what stays local, how the push protocol works, how transfers resume, and how the system
reconciles when two clients have committed to the same branch.

#### 14.1 Why centralized

The remote is the single source of truth for which revision is the latest of a branch, who
can read or write a partition, and what counts as the canonical state of the repository. The
asymmetry is intentional, and several properties depend on it.

- *Access control becomes a single decision point.* Partitions are the access boundary, and
  authorization is bound to a session served by the remote. There is no other party to
  authorize against, so a tenant that is not entitled to a partition cannot reach its
  contents through any other route.
- *Durability has a single owner.* The remote is responsible for storing committed bytes
  durably. A client crash does not lose a pushed revision; a fork-and-merge dispute does not
  end with two clients holding incompatible canonical histories.
- *Sparseness is straightforward.* Clients hold what they need; the remote holds everything;
  the contract between them is "ask for what you don't have." A symmetric peer-to-peer model
  would require a meta-protocol for finding which peer holds which fragments, which is a
  different kind of system.

Centralization is not the same as always-online. Most things a developer does locally -
staging, committing, branching, switching, diffing - run against the local mutable store
and the local fragment cache, with no round-trip to the remote.

#### 14.2 What stays local

A Lore client holds enough state to operate independently of the remote for ordinary editing
work:

- *The working tree.* Files materialized on disk according to the view file.
- *Staged intent.* The set of file changes the user has marked for inclusion in the next
  revision. Staging is recorded as intent in the local mutable store; it does not produce
  fragments.
- *Revisions in flight.* Revisions a developer has committed locally but not yet pushed.
  These exist in the local immutable store and are addressed by hash; the local mutable
  store's branch latest pointer references them.
- *Branch state.* For each branch the client has ever materialized, a name-to-ID mapping and
  a latest pointer in the local mutable store. The client can switch between any of these
  without contacting the remote.
- *A fragment cache.* Bytes for fragments that have been fetched at any point. Subsequent
  reads of those fragments do not hit the remote.

What the client does *not* hold is the canonical answer to "what is the current latest of
this branch on the remote." That answer can be queried at sync time; between syncs, the
client uses its own latest pointer, which may differ from the remote.

#### 14.3 The push protocol

Pushing a revision to a remote is two phases.

*Phase 1: upload fragments.* The client enumerates the fragments referenced by the new
revision (and any ancestors not present on the remote), queries the remote for which of them
already exist there, and uploads the rest. Fragment uploads are independent of one another -
parallel, out-of-order, resumable - and each successful upload makes the fragment available
for any subsequent operation.

*Phase 2: advance the latest pointer.* When all fragments are durable on the remote, the
client issues a conditional put on the branch's latest pointer: "set this branch's latest to
H_new, but only if it currently equals H_old." H_old is the latest pointer the client believed
to be current when it started its push.

If the conditional put succeeds, the new revision becomes the latest of the branch on the
remote and is visible to other clients. If it fails - because another client pushed first
since the local client last synced - the server returns a conflict signal and the client
reconciles its local state with the new remote state before retrying.

The fragments-first ordering is what makes the operation atomic at the "branch is now at this
revision" granularity. A push interrupted between phase 1 and phase 2 leaves the fragments
in the immutable store but does not advance the branch; readers see the prior latest until
the conditional put completes.

#### 14.4 Resumable transfer

Phase 1 of push is incremental and resumable. Each fragment is uploaded as an independent
operation; the remote acknowledges each one as it completes. A network interruption, client
crash, or server restart in the middle of phase 1 leaves the already-uploaded fragments in
the immutable store on the remote. They were valid before the interruption and remain valid
after.

A subsequent push starts by re-querying the remote for which fragments are still missing.
The set of newly-needed fragments is whatever did not finish uploading the first time, plus
anything new the client has produced in the meantime. The push proceeds from there, without
re-uploading what already arrived.

The same property applies to fetches. A sync that pulls a large revision from the remote
fetches fragment by fragment; an interruption leaves the already-fetched fragments in the
local fragment cache, and the resume picks up where it stopped. The fragment cache and the
upload protocol are mirror images in this respect.

#### 14.5 Conflict handling and merge

When a push's conditional put fails, the server is telling the client "the branch has moved
since your last sync." The local revision is now based on a parent that is no longer the
latest on the remote, and the user has to reconcile.

Reconciliation in Lore is a sync followed by a merge, initiated by the user. Sync fetches
the new remote revisions on the branch - those between the client's old latest and the new
remote latest - and produces a *merge revision* whose two parents are the local latest and
the remote latest. The merge revision's tree is the combination of the two parent trees.
Once the merge revision has been committed locally (and any content conflicts resolved by
the user), the client retries the push: the new H_old in the conditional put is now the
remote latest, the merge revision descends from it, and the push advances the remote latest
to the merge revision.

Two clients pushing to the same branch interleave through this protocol. Whichever pushes
first wins the conditional put; the loser syncs, merges, and pushes the resulting merge
revision. Most pairs of parallel changes merge cleanly without user intervention; pairs that
touch the same content in incompatible ways surface as conflicts during the merge step,
where the user resolves them before committing the merge revision.

A push can opt in to *server-side fast-forward merge*. With the option enabled, when the
branch has moved on the remote between the client's last sync and the push, the server
attempts to create the merge revision itself - `parent_self` set to the current remote
latest, `parent_other` set to the incoming revision - and advances the latest pointer to
that merge revision. This succeeds when the parallel changes don't touch the same content.
If the server-side merge encounters a conflict, the push fails with a signal back to the
client that the user should sync and merge locally. Server-side fast-forward merge is an
optimization that lets a successful concurrent push complete in one round trip; it is not
a substitute for client-side reconciliation when there is a real conflict.

### 15. State Detection and Staging

Lore takes the filesystem as the source of truth for file content, treats *staging*
as recorded intent about which file changes to include in the next revision, and
keeps a separate persistent record of which files currently differ from the committed
revision. The split between *what is on disk*, *what has changed*, and *what is meant
for the next commit* is what makes the iteration-and-commit loop fast at repository
scale, and what lets external tools report changes into Lore without an in-core
watcher.

#### 15.1 Filesystem as ground truth

The filesystem is the authoritative source of file content. When Lore needs to know
what is in a file right now, it reads the file. There is no intermediary index, shelf,
or server-side projection that has to be kept in sync. Content is what is on disk;
staging declares which paths to include from what's there.

This is different from two common models in other VCSs.

- *Perforce* requires a reconciliation step. Files modified outside the VCS workflow
  (edited without `p4 edit`, added without `p4 add`) are invisible to status until
  `p4 reconcile` runs and updates the server-tracked state. The user has to think
  about whether the server's view of the working copy matches reality.
- *Git* uses an explicit *index* that sits between the working tree and the committed
  state. Changes to files don't show up in the next commit unless they're added to the
  index first; the index has its own state machine that the user has to manage.

Lore avoids both. There is no separate canonical copy of file content to keep in sync
with the filesystem, and no command needed to make a file's contents "known" to Lore
when the user is ready to commit it.

The cost of taking the filesystem at its word is that answering "what has changed?"
naively requires comparing every file against the committed revision. On a small
repository this is cheap. On a repository with millions of files it is not. The next
subsections describe how Lore makes status fast at that scale without giving up the
filesystem-as-truth model.

#### 15.2 Modification tracking on the tree

Modification state lives directly on the Merkle tree nodes that describe the working
tree. Each node carries a *dirty* flag, orthogonal to the staged flag, indicating
that the file at that path differs from the committed revision. The two flags share a
single per-node action - modify, add, delete, move, or copy - so a node has one
action regardless of whether that action is dirty, staged, or both.

Dirty propagates to parent directories: any directory containing a dirty file is
itself dirty. Marking a single file dirty walks up to the root and stops as soon as it
encounters a directory already flagged, so propagation cost is bounded by the depth
of each marked path rather than the size of the tree. The same flag lets "is anything
dirty under this subtree?" be answered with one bit check rather than a recursive
scan.

Both dirty and staged state are persisted in the local *staged anchor* - a per-instance pointer in
the mutable store to the state tree (content-addressed in the immutable store) that records the
instance's divergence from the committed revision. Reusing one anchor avoids a second
serialization cycle on every status call and lets dirty and staged live as flags on the same nodes
rather than as independent data structures that have to be reconciled.

Dirty state is strictly local. It is never serialized into a revision, never
transmitted to the remote, and never visible to other clients. It describes one
working tree's divergence from a specific committed revision and is meaningful only
on the machine that recorded it.

Deciding whether a given file has changed is made cheap. A file is compared against
its tracked node by size first, then modification time, and only by content hash
when the size matches but the recorded modification time differs. The per-file
modification time is held in the local mutable store, keyed by instance and path,
rather than on the tree node - so the common case (size and mtime unchanged) is
confirmed unmodified without ever reading the file's bytes. This is what keeps a full
scan linear in file count rather than in total bytes: most files are dismissed on
metadata alone.

#### 15.3 Paths into the dirty set

A file becomes marked dirty in one of three ways.

The first is *notification*. A `file dirty` operation marks one or more paths as
modified, classifying the action from the current filesystem state - modify if the
file exists and is in the committed revision, add if it exists and is not, delete if
it is missing but was in the revision. Move and copy variants record their actions
without filesystem checks: the caller has already decided what happened. A
notification costs a flag update on the staged anchor and a short walk up to the
root; nothing else in the tree is touched.

The second is *scanning*. A status scan walks the working tree, compares each file
against the committed revision, and updates the dirty set inline - marking modified
files, clearing dirty on files that have been restored, and pruning dirty bits from
directories whose contents no longer contain any dirty children. The scan is
O(working tree). Its result is persisted: a subsequent status call without a fresh
scan reads the cached dirty set directly and returns instantly.

The third is *verification*. Rather than walking the working tree, it re-examines
only the files already marked dirty: a size change is a modification; otherwise, if
the recorded modification time differs, the content is rehashed and compared;
structural actions (add, move, copy, delete) are modifications by definition. A file
that proves unmodified has its dirty flag cleared and is removed from the report.
Verification is bounded by the size of the dirty set, not the working tree, so it
stays cheap even on a multi-million-file repository. A *reset* modifier can be
combined with a scan to discard the staged anchor first, rebuilding dirty state from
a clean slate.

The three paths feed the same store and cover different latency budgets. A scan can
correct dirty state that drifted because a notifying integration missed an event; a
stream of notifications can keep the dirty set current without ever paying for a
scan; verification cheaply confirms that a tracked dirty file is still genuinely
modified without rescanning the tree. None replaces the others.

#### 15.4 Detection lives outside the core

The reason for separating tracking from detection is that the systems best positioned
to detect filesystem changes already exist outside of Lore.

- IDEs maintain an open-file model and know which files the user has edited.
- Kernel-level file system watchers (FSEvents on macOS, ReadDirectoryChangesW on
  Windows, inotify on Linux) deliver change events for subscribed directories.
- Virtual file system providers - integrations that present a Lore working tree as a
  mounted filesystem - intercept write, rename, and delete callbacks at the
  filesystem layer.

Each of these produces authoritative change information as a side effect of work it
already performs. The historical problem is that they cannot communicate that
information to the VCS. Git's fsmonitor hook addresses this with a side channel; GVFS
for Git keeps a separate modified-paths list maintained by the projection layer;
Perforce sidesteps the problem by requiring explicit checkout.

Lore exposes a single integration target. Any process that observes a change calls
`file dirty`; Lore records it. There is no Lore-internal watcher, no platform-specific
event loop in the core library, and no separate side channel to reconcile. All
consumers - the status command, IDE plugins that surface modified files in their UI,
build systems that decide whether to rebuild - read from the same staged anchor.

This pattern is what makes large-repository workflows tractable. Status without an
explicit scan is constant-time in the size of the dirty set, not in the size of the
working tree. A VFS provider presenting a sparse instance can keep the dirty set
accurate without ever materializing a file just to hash it. A watcher delivering
hundreds of change events per second produces only flag updates on the staged anchor.
The full scan, which would otherwise dominate status latency on a multi-million-file
tree, becomes a recovery mechanism for when an integration has missed events rather
than the default cost of asking "what changed?". External integrations stay external;
the core library stays small.

#### 15.5 Staging as recorded intent

A *stage* in Lore is a set of file paths the user has marked for inclusion in the
next revision. Staging is purely about *intent*: which of the changes currently on
disk should be part of the next commit, and which should not.

The stage records intent in two senses. First, it pins the *path*: the user has said
"I want this file's change in the next commit," not "I want this specific content in
the next commit." Second, it does *not* freeze the content. Between staging and
committing, the user can keep editing the file, and the commit picks up whatever the
file looks like at commit time - not whatever it looked like at stage time.

This matches how iteration actually works. A developer stages a file, keeps editing,
runs tests, fixes, and commits. The commit captures the latest state of the staged
file, not a snapshot taken when the user first ran stage. If the developer wants a
different set of files staged for a particular commit, they unstage and restage; the
underlying content does not need to be moved or re-stored.

Staging does not clear dirty: a file can be dirty, staged, or both. The two flags
answer different questions - "has it changed?" and "will it be in the next commit?"
- and a typical iteration moves through dirty-only (notified or scanned), to
dirty+staged (user has marked it for the next commit), to clean (committed).
Unstaging a still-modified file leaves it in the dirty set; restoring it from the
committed revision clears both.

#### 15.6 Dirty across operations

Dirty state outlives most operations that touch the working tree. Sync to a new
revision preserves dirty flags, so the user's pending modifications travel with them.
Branch switches preserve them by default; a forced switch that explicitly drops
local changes also drops dirty state. Merge, cherry-pick, and revert preserve dirty
via additive flag updates. The principle is that the dirty set represents user work
that has not been committed; nothing short of an explicit reset or a successful
commit should discard it.

Commit is the natural clearing point. A committed file is no longer dirty against
the new committed revision, and its flags are cleared. Files that were dirty but not
included in the commit remain dirty, now relative to the new revision; the staged
anchor is re-parented and persisted so subsequent status calls see them under the new
baseline.

#### 15.7 Fragments at commit time, not at stage time

Staging records intent; committing produces fragments. The split is deliberate.

Hashing and chunking a multi-gigabyte file is expensive. If staging produced
fragments, every intermediate stage of an iterative edit would re-fragment and
re-store the file - the user would pay the cost of full content addressing every time
they decided "yes, this file should be in the next commit," even though they hadn't
yet decided what content to commit. Repeated staging of a file the user hasn't
finished editing would be the normal case, and the content stored would be a sequence
of working drafts, not the final state.

By deferring fragmentation to commit time, the cost is paid once per file per
commit, on the content the user actually decided to commit. Staging is a metadata
operation - a few filesystem-attribute checks, a size-and-modification-time check
(rehashing only when those are inconclusive) to confirm the file is what status
thinks it is, and a stage-list update on the staged anchor. Marking a file dirty is
even cheaper: a flag update, no content read at all. Both remain fast on a repository
with millions of files.

The same separation makes unstage cheap: removing a file from the stage is just
clearing the staged flag; no fragment rewriting, no content migration. Removing a
dirty mark is similarly cheap.

### 16. Fault Tolerance and Atomicity

Every operation in Lore that touches state is structured as a long resumable-partial phase
followed by a short atomic finalization. Expensive work - fragment writes, network
transfers - can fail and be retried without consequence; the visible state transition is a
single conditional put on the mutable store.

#### 16.1 Two operating modes

- *Resumable partial.* The operation is a sequence of independent steps; partial completion
  is a valid intermediate state. If interrupted, work already done is not lost - a retry
  picks up from where it left off. Examples: pushing fragments to a remote, syncing a
  working tree, fetching fragments on demand, materializing a view.
- *Fully atomic.* The operation either completes in full or leaves the repository in its
  prior state, with no observable intermediate. Examples: advancing a branch's latest
  pointer, obliterating a fragment, the local mutable-store update at the end of a commit.

A push uploads many fragments (resumable partial), then issues a single conditional put on
the latest pointer (atomic). A commit creates many fragments locally (resumable partial),
then writes the new revision state and advances the local latest pointer (atomic). The
shape recurs.

#### 16.2 Why immutability makes this tractable

A purely mutable system has to engineer fault tolerance throughout: every operation that
mutates state has to handle "what if interrupted?" and "what does a partial write look
like to readers?" on its own.

In Lore, almost everything is content-addressed and immutable. A fragment write either
produces a fragment with the right hash or it does not; if interrupted, no intermediate
state exists. Operations that need many fragment writes can issue them in any order;
failed writes can be retried, and successful ones remain valid forever. The mutable
surface that's left is small and structured, and every update goes through the same
conditional-put primitive (§9.4) - the only place in the system that has to think about
concurrent writers and atomic transitions.

#### 16.3 Server-side validation and client trust

The server's atomicity guarantee depends on validating every push end-to-end before
advancing the latest pointer: graph integrity (the revision's immediate parents resolve),
partition membership (every referenced fragment is present in the session's partition), and
revision-state consistency (all tree fragments resolve). If validation fails, the fragments may
already be in the immutable store but the latest pointer is not advanced; the new revision is not
visible to anyone.

Clients depend on this. Hash validation of fetched fragments is end-to-end and free (the
content address proves the bytes), but graph-level validation is what the server has
already done. This is the trust boundary in §3.2: clients hash-validate content, and trust
the server to have validated graph structure. A misbehaving server could in principle
present a malformed graph; the defense is that the server rejects malformed pushes, so
clients fetching from a correctly-operating server never see one.

#### 16.4 What can go wrong and what each failure looks like

Failures have well-defined user-visible shapes:

- *Network interruption during push.* Some fragments are uploaded; the latest pointer has
  not advanced. A re-run resumes from where the interruption occurred. The branch on the
  remote is unchanged.
- *Client crash during commit.* Local fragments may have been written; the local latest
  pointer has not advanced. The next commit re-creates fragments (cheap, because the
  immutable store deduplicates) and completes.
- *Server crash during a conditional put.* The crash is either before the put (no change)
  or after (latest pointer advanced, revision visible). No intermediate state.
- *Concurrent push collision.* The conditional put fails; the client reconciles via
  sync-and-merge (§14.5). The local state is unaffected.
- *Validation failure on push.* The server rejects the push. Fragments uploaded in phase 1
  may be in the immutable store, but no branch latest pointer references them; the failed
  revision is unreachable, and the unreferenced fragments can be obliterated if desired.

In each case, the user-visible failure is "the operation did not happen, and nothing else
changed." There is no in-between state to clean up.

### 17. Access Model, Data Isolation, and Multi-Tenancy

Lore is designed so that multiple unrelated repositories can share a single backend
deployment safely. The threat is mutually distrustful tenants on shared storage; the
defense is a layered access model rooted in partitions. This section explains the threat,
the partition boundary and how it's enforced, the authentication/authorization mechanics,
the "knows the hash" attack and the ownership-proof protocol, server-side validation, and
the side-channel discipline that keeps the protocol from leaking what the access model
hides.

#### 17.1 The threat model

Lore can host many unrelated repositories on one backend deployment. The arrangement is
operationally efficient but creates a security obligation: tenants who don't trust each
other must not be able to reach each other's content, even though the storage substrate
underneath is shared.

The threat model assumes mutually distrustful tenants. A tenant might:

- Try to read content from another tenant's partition.
- Try to write content into another tenant's partition.
- Try to learn what content exists in another tenant's partition (does this hash exist?
  does this name resolve?).

Lore's job is to defeat all three through the access model, even when underlying storage
is shared and even when content addressing means identical bytes occupy a single physical
slot. What is *not* in the threat model: an adversarial server (§3.2 non-goal),
side-channel attacks below the protocol level, and physical access to backend storage.

#### 17.2 Partitions as the access boundary

A partition is a 16-byte opaque identifier; every fragment and every mutable-store key
lives in exactly one partition. Authorization on a session is bound to a partition, and
content lookups are partition-scoped: a session bound to partition A cannot read or write
any storage object in partition B, even if both happen to share content.

The partition is *derived from the authenticated session*, not asserted by the client. The
client sends a request; the server resolves the partition from the session's authorization
scope and uses that. A client cannot bypass the boundary by naming a different partition;
the partition is what the server says it is, based on the authenticated identity.

#### 17.3 Authentication

Lore uses JWT bearer tokens, carried over both QUIC and gRPC. A token encodes a user
identity, a resource list, an expiry, and a signature. On session establishment, the
server verifies the signature, checks expiry, and resolves the resource list to determine
which partitions and operations the session is authorized for.

The same token format works across transports. The protocol carries the JWT in
transport-appropriate envelopes (a header in gRPC, an authorize payload in QUIC), and the
server validates it the same way.

#### 17.4 Authorization scope

A token's resource list grants access to specific repository partitions: the partition is the
resource identifier. Each entry pairs a partition identifier with a permission set (such as
read, write, obliterate, admin) describing what the bearer is authorized to do in that
partition.

Operator-level access uses wildcard resource entries granting blanket access across all
partitions in the deployment. This is for service accounts running admin and replication
tasks; user-level tokens never carry wildcards.

The session's effective permissions for a given partition are the intersection of the
token's resource grant and the server's policy. A token granting read access on partition
A cannot, regardless of how the request is shaped, perform a write on partition A.

#### 17.5 Why content addressing alone is not enough

Content addressing makes deduplication automatic: two clients producing the same bytes
produce the same hash. But if "the hash exists" implied "you can read the bytes," a tenant
who learned a hash from another tenant's partition could fetch it - the query would find a
match and content addressing would happily serve the bytes regardless of who originally
put them there.

This is the *knows the hash* attack. Hash discovery is not hypothetical: hashes show up in
logs, build artifacts, error messages, shared dependencies; an adversary doesn't need to
brute-force them. A multi-tenant system that ignored this attack would leak content the
moment any tenant learned a hash from another.

Lore's response is to keep storage dedup *underneath* the access model, not above it. Two
partitions storing the same bytes share the underlying storage, but the access check is
partition-scoped: a session bound to partition A cannot fetch a fragment by hash unless
that fragment is registered in partition A.

#### 17.6 Put requires bytes; Copy is the cross-partition shortcut

Two operations register a fragment in a partition: Put and Copy.

*Put* always requires the bytes. The client sends the address it wants to register and the
payload; the server stores the payload (or recognizes that it already has the bytes
internally) and registers the address in the client's partition. Whether the same hash
exists elsewhere in the storage layer is irrelevant to the protocol - the payload must
accompany the Put. This is the defense against the knows-the-hash attack: hash knowledge
alone is never sufficient to register a fragment.

*Copy* is the cross-partition shortcut. If the session has access to both partition X and
partition Y, and a fragment is registered in X, the session can ask the server to copy
the registration into Y without re-transmitting the bytes. The server validates both
authorizations and that the hash exists in the source partition, then registers it in the
destination partition. The client never re-uploads the payload.

The split is economical on the network - bytes traverse the wire only when needed - but
strict on access: Copy requires authorization on both ends, so it cannot reach into a
partition the session does not already have.

#### 17.7 Context as identity, not authorization

A context (§7.4) is a 16-byte tag carried alongside a content hash for tracking identity
within a partition - file ID for move and copy, obliteration scope, similar
version-control bookkeeping. Context is *not* an access boundary. Two fragments with
identical hashes but different contexts are distinct entries that share storage; they do
not constitute different access scopes. Authorization is exclusively at the partition
level; context is metadata above it.

#### 17.8 Server validation on push

The server validates every push end-to-end before advancing the latest pointer (§16.3).
For multi-tenancy in particular, validation includes a partition-membership check on every
fragment referenced by the new revision: each address must be reachable from the session's
partition. A push that would reference fragments in another partition fails validation,
and the latest pointer is not advanced.

This is how the access model survives revision-graph operations. Linking, cherry-picking,
or merging cannot smuggle in a fragment from another partition; the server checks
partition membership for the whole graph before accepting it.

#### 17.9 Side-channel discipline

Even within the access model, what the protocol *says* leaks information. The protocol's
return shapes are designed to reveal exactly what is needed for legitimate operation and
no more.

The fragment-existence query is scoped to one partition (the one the session is bound to)
and returns one of four values:

- `FoundInContext` - the fragment exists in the queried partition with the supplied
  context. The normal "is it already cached / committed" answer.
- `Found` - the fragment exists in the queried partition with a different context. Two
  fragments with the same hash but different contexts are distinct entries (§7.5); this
  signal tells the client the bytes are present in the partition even though the specific
  (hash, context) pair is not.
- `NotFound` - the fragment is not in the queried partition.
- `Unknown` - state could not be determined for transient reasons.

There is no cross-partition signal in the query: a session asking about a hash in
partition A learns only what is in A. A client that wants to know whether to Put or Copy
across two of its own partitions queries each separately.

Timing is part of the contract. Lookups return in roughly constant time regardless of whether
content exists, to avoid leaking existence through timing - the cold-storage path takes the
same order of magnitude in execution time whether the content is found or not.

Error codes follow a strict precedence order: invalid command, then not authorized, then
slow down, then failed, then not found. A query against a partition the session is not
authorized for returns "not authorized" before any existence check happens, so error codes
do not leak existence information past the authorization boundary.

### 18. The Storage Protocol

Lore exposes the storage subsystem over the network through a single logical command set
served on two transports: a binary QUIC protocol and a gRPC protocol. Both expose the same
operations with the same semantics. This section explains the shape of the protocol, how
pipelining and multiplexing work, and why the wire format differs from the on-disk format.

#### 18.1 Two transports, one logical surface

The protocol's command set is small and the same on both transports:

- *Authorize* - establish or end a session.
- *Get*, *GetMetadata*, *Put*, *Query*, *Verify*, *Copy* - operations on the immutable store:
  read a fragment, read only its metadata, write one, query existence, verify content, copy a
  fragment across partitions.
- *MutableLoad*, *MutableStore*, *MutableCas* - the `load`, `store`, and `cas` operations on
  the mutable store: read a key, write a key, compare-and-swap a key.
- *GetResolved*, *PutResolved* - span both stores, using the `Resolve` key type. *GetResolved*
  resolves a mutable key and returns the immutable blob it names, so a key lookup and the read
  it feeds share one round trip instead of two. *PutResolved* is its write side: it stores a
  fragment and publishes a key naming it in one round trip. The mapping is published only once
  the content behind it is stored, which is what distinguishes it from writing the same key type
  with *MutableStore* directly.

QUIC and gRPC carry these commands with the same arguments and return the same answers.
The QUIC transport (ALPN `lore-storage/0.4`) is binary and low-overhead, designed for
high-throughput fragment transfer. The gRPC transport is HTTP/2 with protobuf-encoded
messages, useful where QUIC is not available or where existing HTTP-based infrastructure
(load balancers, proxies, observability) is the path of least resistance.

A server can serve both transports simultaneously; clients pick whichever fits their
deployment. No operation is reachable on one transport but not the other.

#### 18.2 Pipelining and multiplexed sessions

A QUIC connection carries up to eight bidirectional streams; two are reserved as priority
for control commands, the rest carry fragment traffic. On each stream the client may
pipeline commands: send command N+1 before reading command N's reply. Replies may arrive
out of order and on different streams than the originating request. A 12-byte command
header carries the opcode, payload size or error status, a client-allocated `command_id`,
and a `session_id`; the `command_id` correlates each reply back to its request.

Within a connection a client opens *sessions*. A session binds a repository (partition), a
correlation ID, and a user identity to a server-assigned `u32` session ID. Many concurrent
sessions per connection are supported, allocated monotonically and not reused within a
connection. The Authorize command creates and ends sessions; every subsequent storage
command carries the session ID in its header.

The combination - several streams, pipelined commands per stream, many sessions per
connection - lets a single connection saturate available bandwidth without head-of-line
blocking. A push uploading thousands of small fragments and a fetch streaming megabytes
of data can share one connection without one starving the other.

#### 18.3 Wire format: one fragment per command

On the wire, fragments travel individually, one per command. Each fragment is the payload
of a single Get response or Put request, with no batched envelope above and no notion of a
packfile in the protocol. The maximum payload size for a single command matches the
fragment-size threshold (256 KiB), so each fragment fits in one command-and-response
round.

Bundling at the protocol layer would defeat per-fragment parallelism (clients fetch
concurrently across streams), per-fragment dedup queries (the client asks "do you have
this fragment?" individually), and per-fragment resumability (an interrupted transfer
leaves earlier fragments intact). The packfile structure used on disk (§7.6) is a
separate concern; the protocol layer translates between them.

### 19. Obliteration and Data Lifecycle

Most operations in Lore are append-only - revisions are immutable, branches archive
without rewriting history, fragments stay in the store once written. Some scenarios,
though, demand actual byte-level removal: legal holds, leaked secrets, regulatory
deletion. This section explains why removal is hard in a content-addressed system, how
Lore handles it (fragment-level obliteration scoped by file ID), and what obliteration
cannot do.

#### 19.1 Why removal matters

There are scenarios where data must actually leave the store, not just become unreachable
through some branch:

- *Legal hold release.* Data was retained under a hold; the hold has expired; the
  organization is required to destroy the bytes.
- *Secrets accidentally committed.* A credential was checked in. Rotating the credential
  is the immediate fix; getting the old credential out of the repository is the follow-up.
- *Bulk data cleanup.* A retired project's large binaries no longer need to occupy
  storage.
- *Right-to-erasure requests.* Regulatory regimes (GDPR-style) require deletion of
  specific personal data on request.

In each case, "the address can no longer reach the bytes" is the actual goal. Lore
supports genuine removal, and does so without breaking the content-addressed model the
rest of the system relies on.

#### 19.2 Why naive deletion is hard

The obvious approach - "find every reference to this fragment and delete them" - runs
into two well-known traps in any content-addressed system.

*The reference-counting trap.* In a deduplicating store, a single fragment is referenced
by every revision, every link, every chunk list that contains its bytes. Knowing whether
a fragment is safe to delete would require maintaining a reference count or periodically
walking every revision in every branch. Reference counts in a distributed multi-tenant
store are notoriously hard to get right; full walks are linear in repository size and
impractical at scale.

*The history-rewrite trap.* In a Merkle tree, any change to a fragment's bytes changes
its hash. A revision's tree references the old hash; if that fragment is rewritten, the
revision becomes inconsistent with its own tree. Rewriting the revision to reference a
different fragment changes the revision's hash, which changes its descendants' hashes,
which changes every branch that contains them. The "fix" propagates all the way up the
graph, breaking every signature in its path.

Lore avoids both: it neither counts references nor rewrites history. The mechanism is
fragment-level obliteration.

#### 19.3 Fragment-level obliteration

Obliteration in Lore removes a fragment's *payload* while keeping its *address* intact in
the store's index. The fragment's metadata records that the payload has been obliterated.
A reader who asks for the fragment by address gets a typed "this fragment was obliterated"
response, with the metadata flags indicating the state, rather than corrupted bytes or a
generic not-found.

Two flags carry the obliteration state:

- `PayloadObliterating` - obliteration is in progress; the payload is being torn down but
  the operation has not completed.
- `PayloadObliterated` - obliteration has completed; the payload is gone.

Together they form a small state machine: a fragment is either present, mid-obliteration,
or obliterated. Readers see the current state on every fetch and behave accordingly: a
present fragment yields its bytes; an obliterating or obliterated fragment yields a typed
absence.

The address survives because revision graphs point at it, and removing the address itself
would break every revision that ever referenced it. The bytes are gone because that is
the actual goal.

#### 19.4 File ID as the obliteration scope

Reference counting fails because there is no efficient way to enumerate "every reference
to a hash" across an entire repository or deployment. Lore sidesteps the problem with the
*context* field of fragment addresses (§7.4): for fragments backing a file's content, the
context is the file's stable per-file ID.

When a file is obliterated, the storage layer obliterates every fragment whose address
matches the file's context. The lookup is by context, not by walking revisions; the cost
is proportional to the number of fragments belonging to the file, not to the size of the
repository or its history. Different files that happen to share content do not share a
context, so obliterating one file does not affect the bytes of another even though they
share underlying storage.

This is why context exists as a separate field of the address. It carries enough identity
to scope obliteration without compromising the dedup that makes content addressing useful.

#### 19.5 Two-phase obliteration

Obliteration is a two-phase operation, reflected in the two flags. Phase one transitions
the fragment from present to `PayloadObliterating`: the metadata is updated to record
that obliteration is in progress, and the payload is scheduled for deletion. Phase two
transitions it to `PayloadObliterated`: the payload is gone, the metadata records it,
and readers see a typed absence.

The two-phase structure is what makes obliteration crash-safe. A crash between the two
phases leaves the fragment in `PayloadObliterating`; on recovery, the obliteration is
completed. There is no state where the metadata claims the bytes are present but the
bytes are gone, or where the bytes are gone but no metadata records the obliteration.

#### 19.6 Implications for revision integrity

Obliterating a fragment does not invalidate the revisions that reference it. The hash in
each revision's tree still resolves to the same address; the tree still walks;
verification still produces the right structural results. What changes is what comes
back when a reader fetches the *bytes*: instead of the original payload, the reader gets
a typed "obliterated" response.

To a downstream consumer this looks like a present-but-unreadable fragment. A diff that
crosses obliterated content shows "this content was obliterated" rather than producing
bogus output. A sync that needs the bytes for materialization cannot complete and reports
the failure cleanly. A clone of a revision whose content has been obliterated still walks
the tree but stops at the obliterated fragments.

The integrity of the content-addressed graph is preserved: hashes still match, signatures
still verify, the revision chain is still cryptographically linked. The only thing
missing is the bytes that were removed - which is exactly what obliteration is meant to
do.

#### 19.7 What obliteration cannot do

Obliteration removes payload bytes from storage. It does not, and cannot, do several
related things:

- *Recover an old hash.* The hash was derived from the bytes; the bytes are gone. The
  hash can no longer be re-derived even though it is still recorded in revisions.
- *Un-leak data already cloned by a tenant.* Once a fragment has left the deployment - in
  a clone, a backup, an export - obliteration on the deployment cannot reach those copies.
  Defense against leakage is upstream of obliteration, in access control and in policies
  against extraction.
- *Forget the fact that a revision existed.* The revision state is itself a fragment with
  its own hash. Obliterating its content fragments removes the bytes; the revision's hash
  and the surrounding graph still record that some revision was committed.
- *Substitute different content.* Obliteration removes; it does not edit. A fragment
  cannot be obliterated and replaced with different bytes under the same hash, because
  the hash is derived from bytes that are no longer recoverable.

For the cases obliteration does not cover, the system relies on access control (§17),
secrets-rotation hygiene (out of scope for the storage layer), and defense in depth
around the entire pipeline.

### 20. Replaceable Backends

Lore's storage subsystem is defined by two trait interfaces - `ImmutableStore` and
`MutableStore` - and any combination of implementations that satisfies them is a valid
deployment. The reference implementations cover local-file and AWS scenarios; third
parties are free to add their own. This section explains the implementations Lore ships
today and what the interface boundary requires.

#### 20.1 Immutable store backends

The `ImmutableStore` trait exposes a small set of operations: write a fragment, read a
fragment by address, query whether a fragment exists in a partition, obliterate, and copy
a fragment between partitions inside the same store. Any backend that satisfies these
with the right semantics is a valid immutable store.

Lore ships with:

- *Local file store.* Backs clients, the deployment's edge cache tier, and any server
  layer that wants a fast local store. Fragments live in packfiles - large append-only
  files holding many fragments back-to-back, with mmappable indexes (§7.6) - bucketed
  and fanned out across the filesystem. Designed for high-throughput sequential writes
  and constant-time random reads. The same implementation that runs in a developer's
  local cache runs in the edge tier of a deployment.
- *AWS S3 store.* For server deployments that want object-storage durability. Each
  fragment maps to an S3 object addressed by hash; the store handles batching, retry,
  and consistency on top of S3.
- *Replica wrappers (`ReplicatedStore`).* Compose an immutable store with one or more peer replicas
  reached over the network, replicating both reads and writes. Reads at one peer pull warm
  fragments from neighboring peers when the local copy misses; writes at one peer
  propagate to the others, so a fragment just put on peer A is immediately available at
  peer B without going back to the durable upstream store. The pattern increases
  hot-fragment hit rate across the fleet and avoids the latency cost of fetching newly
  written content from the durable tier.

Deployments can mix these. A local fragment cache backed by a network replica backed by
a durable S3 store, all stacked behind one `ImmutableStore` interface, is a typical
production layout.

#### 20.2 Mutable store backends

The `MutableStore` trait exposes a narrow API: `load`, `store`, `cas`, `list`, each
taking a typed key. The `cas` (compare-and-swap) operation is what gives Lore atomic
state transitions (§9.4); every backend must implement it correctly.

Lore ships with:

- *Local file store.* File-backed key-value store with a bucketed layout, using
  filesystem locking for the `cas` primitive. The default for clients and
  small servers.
- *AWS DynamoDB store.* DynamoDB's conditional-write support maps directly onto the
  `cas` primitive. Suitable for multi-region server deployments where the
  mutable store is the only true serialization point and DynamoDB's consistency
  guarantees are the right fit.

#### 20.3 The interface boundary

What the core library guarantees:

- The address structure (hash + context), the fragment metadata format, the
  obliteration flags, and the serialized form of mutable-store keys.
- The semantics of every trait operation: what counts as success, what each error
  category means, what guarantees the caller can rely on.
- The conditional-put precondition and the atomicity expected of it.

What backends must implement:

- The trait operations with the documented semantics.
- Crash safety and durability guarantees appropriate to the deployment.
- Partition scoping in storage: the access boundary is enforced a layer above the
  backend (§17), but the backend must keep partitions isolated in its on-storage
  layout, so that a leak through one layer does not silently leak through another.

A third-party backend - a different blob store, a different KV system - is a question of
implementing the two traits. The rest of Lore does not change.

### 21. Backend Scalability

A Lore deployment scales horizontally on the read path, replicates aggressively at the
edge, and concentrates the consistency-sensitive work into the mutable store's
conditional-put primitive. This section walks the scaling axes, why most of the workload
parallelizes trivially, where the bottleneck is, how tiering and replication absorb
load, and how the deployment signals back-pressure when it cannot.

#### 21.1 Scaling axes

A deployment scales on several independent axes simultaneously:

- *Tenants per deployment.* How many distinct organizations or projects share one
  backend.
- *Repositories per tenant.* How many partitions one tenant occupies.
- *Branches per repository.* How many concurrent lines of work live in each partition.
- *Fragments per repository.* The total content footprint, in fragments.
- *Concurrent users.* Independent humans and tools working simultaneously.
- *Peak concurrent sessions.* Authorize-bound sessions in flight, including those held
  by long-running operations.
- *Push throughput.* Writes per unit time the deployment must accept.
- *Sync throughput.* Reads per unit time the deployment must accept.

These axes are not independent in cost, but they are independent in *shape*: more
fragments increase storage and read traffic; more branches increase mutable-store
contention only when they're being pushed to; more concurrent users increase session
count and read traffic but not write contention. The system has to scale gracefully on
each axis without one starving the others.

#### 21.2 Stateless reads

The read path is essentially stateless. A fragment lookup is "give me the bytes for
address A in partition P": no per-session state, no cursor, no transaction. Any server
that can reach the storage subsystem can answer the request, and answers are
content-addressed - the same response from any server is the same bytes.

This makes reads embarrassingly parallel. Read traffic spreads across however many
servers the deployment provisions, and any one of them can answer any request. There is
no "primary" or "leader" for the read path, and no need for session affinity beyond
what the underlying transport already provides.

The same property applies to fragment writes into the immutable store: the address is
determined entirely by the bytes, two writes of the same content produce the same
address, and concurrent writes from different clients to different fragments do not
contend.

#### 21.3 The mutable store as the consistency hot spot

Where reads and fragment writes parallelize, mutable-store operations cannot. The
conditional-put primitive (§9.4) is the only true serialization point in the
architecture: when two clients both push to the same branch, the mutable store decides
which one wins. That decision is per-key, not per-deployment - two pushes to different
branches do not contend; two pushes to different repositories contend even less.

Per-branch contention is the natural granularity. A repository with many branches
parallelizes across them; a single branch with many simultaneous pushers is the worst
case. In practice the worst case is rare - branches typically have a single primary
contributor at any moment - but the system handles the contended case correctly:
failed conditional puts surface as conflicts (§14.5), which clients reconcile via
sync-and-merge.

Sizing the mutable store is the most consequential capacity decision in a deployment.
Reads and fragment writes can be scaled by adding servers; latest-pointer advances
ultimately funnel through whichever backend implements the conditional put (§20.2).

#### 21.4 Hot/warm/cold tiering and edge servers

Production deployments tier storage to balance latency against cost.

- *Edge servers* run near clients, holding a high-throughput local cache (§20.1) of the
  fragments those clients have recently asked for. Most reads land here; in a typical
  deployment the edge tier serves 90% or more of fragment traffic without ever touching
  upstream.
- *Primary servers* hold the canonical state of the deployment - the mutable store and
  recent immutable content - co-located with the durable storage. Edge servers
  delegate misses upstream to a primary in their region (the `ReplicatedStore`
  pattern), so cross-region latency is paid once per request rather than compounded
  across many sequential operations.
- *Cold storage* is for fragments that are unlikely to be read soon. The S3 backend
  (§20.1) is one cold tier; deployments can offload older content to it and rehydrate
  on demand.

The tiering is invisible to clients. A request goes to whichever endpoint of the remote
is nearest, the deployment fetches from the closest tier holding the bytes, and the
response comes back. The client never knows whether the answer came from edge, primary,
or cold; tiering is a deployment decision, not a protocol feature.

#### 21.5 Replication, failover, and read replicas

Within a region, multiple peer servers replicate fragments and mutable-store state to
each other (§20.1, §20.2). A fragment put on one peer propagates to the others, so
neighboring servers can answer reads for newly-written content without round-tripping
to the durable upstream. Mutable-store state replicates similarly, with conditional-put
semantics preserved across peers.

Failover follows from this. If a server is taken offline - maintenance, failure,
scale-down - its peers continue to answer requests. Clients reconnect to whichever
endpoint is reachable; sessions are per-connection, and a new session resumes
operations without the client knowing a server has changed.

Read replicas are the dedicated-read variant of the same pattern. A deployment can
provision servers that participate in replication for reads but do not accept writes,
absorbing read traffic without growing the write tier.

#### 21.6 Backpressure: the `SlowDown` signal

Every storage operation can return a `SlowDown` error in addition to its normal
results. The signal means "the server is overloaded; back off and retry." Clients
implement exponential backoff and retry against the same or a different endpoint;
servers raise the signal when they detect they are at capacity (queue depth, pending
operations, downstream throttling).

`SlowDown` is the deployment's mechanism for absorbing surge load without dropping it.
A deployment with a small primary tier and a large edge tier will see most surges
absorbed by edge caching; rare surges that reach the primary are distributed across
replicas; rare surges that exceed the primary's capacity surface as `SlowDown` to the
client, which retries with backoff. The pattern is monotonic: more load slows clients
down; it does not return wrong answers.

### 22. Sub-Repository Links

§5.6 introduced links architecturally: a link is a tree node with a flag and a target
address; each linked repository is its own partition with its own access control; the
link is intrinsic to the committed revision and travels with it. This section unpacks
the parts that do not fit in the architectural overview: how Lore's links differ from
the submodule patterns of other systems, how traversal works at the path level, and the
operations that involve a single link in isolation.

#### 22.1 The submodule problem

Multi-repository composition has been attempted in nearly every version-control system,
usually as an after-the-fact bolt-on. Git submodules and Mercurial subrepos are the most
familiar; both share a similar pattern and a similar reputation: useful in principle,
fragile in practice.

The recurring problems:

- *Pin churn.* Each composing repository pins a specific revision of each linked
  repository. A change to the linked repository requires a corresponding pin update in
  every composing repository. Coordinating updates across many composers is manual and
  error-prone.
- *Workflow asymmetry.* The composing repository and the linked repository have
  different lifecycles, different branches, different histories. Operations that are
  natural in one repository - creating a branch, switching, merging - require explicit
  coordination across all linked repositories.
- *Tooling discontinuity.* Cloning, building, and reviewing tools either know about
  submodules and treat them specially, or they do not and produce wrong results. The
  set of tools that handle submodules correctly is always smaller than the set of
  tools the user actually needs.
- *Half-presence.* Submodules can be in odd states - uninitialized, detached, in a
  different revision than the pin. Each state is a special case.

Lore's links target the same use case but commit to a different model. They are
intrinsic to the parent revision rather than bolted on; they participate in normal
version-control operations as transparent boundaries; and the access-control story
(§5.6) is designed in rather than overlaid.

#### 22.2 Subset selection, path remapping, and transparent traversal

A link resolves two paths in addition to the linked repository ID and revision: the
*source path* inside the linked repository, and the *link path* in the parent's tree.
The link path is the position of the link node in the parent's tree; the source path
is determined from the link node's target in the linked repository's tree - neither is
stored as a literal string. The link mounts the subtree at the source path of the
linked repository - not necessarily the linked repository's root - at the link path in
the parent. A linked repository's `lib/widgets` directory can appear as
`vendor/widgets` in the parent; the mount is a subtree of the linked content remapped
to a different location in the parent's tree.

When the version control subsystem walks a path that crosses a link node, it switches
context to the linked repository's state, deserializes that state, applies the
source-path offset to translate the parent-relative path into a linked-repository path,
and continues resolution there. The user sees a single seamless tree; the storage
subsystem handles the boundary and the remapping invisibly. A path like
`parent/vendor/widgets/button.rs` walks through the parent's tree to the link node,
remaps to `lib/widgets/button.rs` inside the linked repository, and resolves to the
file there.

Sparse views compose with links naturally. The user's view file (§12.2) applies to the
final remapped path in the parent's tree: a view that materializes only
`parent/vendor/widgets/...` pulls in just that subtree of the linked repository,
without materializing the rest. Tools that are not link-aware do not need to be: the
seamless tree, the view, and the link boundary all interact through the same
path-resolution machinery. Writes through the link path (staging, committing) work the
same way at any depth.

#### 22.3 Pinning, auto-follow, and branch mirroring

A link in a parent revision always encodes a *specific* revision hash of the linked
repository. The link is intrinsic to the parent's revision (§5.6, §22.2): every
clone of a given parent revision sees the same linked content, because the linked
revision is recorded in the parent's tree. The pin does not silently track a moving
target - that would break the parent revision's immutability.

What *auto-follow* controls is whether the link participates in branch *operations* on
the parent, not whether the pin moves on its own. Two modes:

- *Auto-follow enabled* (the default). When a new branch is created in the parent,
  Lore creates a corresponding branch in each auto-follow-enabled linked repository,
  starting at the link's currently-pinned revision. The link records which branch in
  the linked repo it belongs to; subsequent operations on the parent's branch (merge,
  pin update) flow through to the same-named branch in the linked repository. This is
  what gives multi-repo workflows their branch-and-merge symmetry: a parent branch
  named `feature-x` automatically gets a `feature-x` in each linked repository, with
  the same metadata.
- *Auto-follow disabled* (`DisableAutoFollow` flag). The link does not grow new
  branches when the parent does, and is not eligible for cross-link merge operations.
  This is the right choice for vendored third-party content - the parent's branch
  operations should not pollute the linked repository's branch space.

Either way, the parent revision pins a specific revision of the linked content.
Updating the link to a different revision is an explicit `link update` operation that
produces a new parent revision; the previous parent revision still pins the previous
linked revision.

#### 22.4 Link-scoped commit and single-link merge

By default, commits and merges in Lore cascade through the parent and any linked
repositories that have changes. Two operations are scoped to a single link:

- *Link-scoped commit.* `commit --link <path>` commits staged changes in one linked
  repository only, advancing that link's pin in the parent's staged state without
  touching the parent's other links or content. The parent itself is not committed;
  the new pin shows up in the parent's next commit. This is useful when resolving
  conflicts in one linked repository, or when making incremental progress on a link
  before committing at the parent level.
- *Single-link merge.* `branch merge start <branch> --link <path>` runs a merge through
  one link's mounted repository, leaving the parent and other links untouched.
  Conflicts are resolved at the link's mount path; the parent's link pin updates to
  the resulting merged revision when the merge completes.

Both operations exist because the cascading default isn't always what the user wants.
Working on a single linked repository at a time, with the parent quiet, matches the
realistic workflow of a developer focused on one component.

#### 22.5 Access control

Each linked repository is its own partition with its own access control; the link
boundary is the access-policy boundary. A user with access to the parent but not to
the linked repository sees the link node but cannot descend into it. The full
architectural story is in §5.6; the operational point in this section is that pin
updates, auto-follow, and link-scoped operations all run subject to the access policy
of the *linked* partition - the user must have appropriate permissions in the link's
own partition, not just in the parent.

### 23. Layers

§5.7 introduced layers as the local counterpart to links: an overlay of one
repository's content onto another at a path, applied locally rather than stored in the
parent's revision. This section unpacks the parts §5.7 didn't reach: what overlay
problems layers solve, how the local configuration stores subset-and-path information
the same way a link encodes it in a tree node, how layers decide which revision of the
source repository to materialize, and how staging, commit, and per-layer revision
messages work when several layers move together.

#### 23.1 The overlay problem

The overlay shape recurs across many workflows. A developer wants to overlay a
personal asset library onto a project tree without committing the overlay path into
the project. A CI pipeline wants to mount an additional set of scripts on top of a
clean clone for a build run, without polluting the parent revision. A release engineer
wants a private build-tools repository mounted beneath a public source tree on the
build machine. None of these uses should appear in the parent's history; all of them
want the path-resolution machinery that links provide for committed dependencies.

Layers are this overlay model. They are configured per machine, applied at
materialization time, and absent from every revision the parent ever commits. Two
machines on the same parent revision can hold completely different layer
configurations, and neither of them changes what gets committed - that is the contract
distinguishing layers (§5.7) from links (§22).

#### 23.2 Subset selection, path remapping, and traversal

A layer mounts a subset of one repository at a path in another, with path remapping,
the same shape as a link (§22.2). A layer records:

- The *target path* in the parent's tree, where the layer appears.
- The *source repository* identifier.
- The *source path* inside the layer repository, where the mounted subtree starts.
- A *current revision* of the layer repository (the revision actually materialized).
- A *staged revision* (the layer's revision in the parent's staging context).
- An optional *metadata key* (see §23.3).

Path resolution across a layer mount works the same way as across a link: walk the
parent's tree to the layer's target path, switch context to the layer repository,
apply the source-path offset, continue resolution there. Sparse views and ignore
files (§12.2, §12.3) compose with layers the same way they compose with links - the
user's filters apply to the final remapped path in the parent's working tree.

The difference is where the configuration lives. A link's target is encoded in the
parent's tree node and travels with the revision. A layer's configuration lives in a
local file under the parent's working directory; it is read at materialization time
and ignored by everything that walks the committed tree.

#### 23.3 Metadata-keyed revision matching and branch auto-follow

A layer must decide which revision of its source repository to materialize at any
given moment. Two mechanisms cover this.

*Metadata-keyed matching.* When a layer is configured with a metadata key, the system
looks up the layer-repository revision whose metadata at that key matches the parent's
current revision metadata at the same key. A typical example: the parent records a
build identifier or release tag in its revision metadata; the layer repository
records the same value on the matching revisions; the layer materializes whichever
layer-repo revision corresponds to the parent's current build. The matching is a
metadata-value search across recent layer revisions, not a branch lookup, and works
across arbitrary correspondences the metadata captures.

*Branch auto-follow on switch.* When the parent switches branches, layers create
matching branches in their source repositories if those don't already exist, and
follow them. Switching the parent's branch automatically switches the layer to the
corresponding branch's latest content; the parent and the layer move together at the
branch level. This keeps multi-team workflows coherent: when the parent team starts a
new release branch, every machine that uses the project's standard layer set picks up
matching branches in each layer without manual reconfiguration.

#### 23.4 Per-layer staging, commit, and revision commit messages

Layers participate in staging and commit operations alongside the parent. By default,
`stage` and `commit` cascade through the parent and any layers that have changes in
the staged paths. Three operations narrow the scope to a single layer:

- *Per-layer staging.* `stage .` against a parent path that overlaps a layer
  populates that layer's own staged state. Each layer has its own working tree, view,
  and conflict resolution; the same path can be staged in either the parent or a
  layer depending on which side the change actually belongs to.
- *Layer-scoped commit.* `commit --layer <path>` commits staged changes in one layer
  independently, advancing that layer's current revision in the local layer configuration without
  touching the parent or any other layers. The parent itself is not committed; the
  parent's next commit will see the layer's new revision in its staged context.
- *Per-layer revision commit messages.* When committing across multiple layers, each layer
  can carry its own revision commit message - either via an interactive prompt or the
  `--layer-message <path> <message>` form. A single high-level commit produces a
  parent revision and one revision per affected layer, each with its own message.

The mode reflects the realistic workflow: layers are usually long-lived overlays that
evolve at their own pace, and the system supports working on one at a time without
forcing the parent or other layers to come along.

#### 23.5 Access control

A layer's source repository is its own repository, with its own partition and its own
access control (§17). Materializing a layer goes through normal authorization on the
source partition; a user without the necessary permissions cannot materialize the
layer's content even though the layer configuration sits in their local working
directory. Different machines configured with the same layer can hold it under
different access identities - one user with read access, another with read-write.

Because layers are local configuration, they do not change the access model. They
shape what is mounted on a particular machine; the bytes still flow through the
storage subsystem and the same authorization checks that govern any other access to
the same source repository.

### 24. Shared Stores and Instances

A *shared store* in Lore is a single on-disk store - both the immutable and the mutable
store - that can be referenced by multiple working directories at the same time. Each working
directory is an *instance*, with its own working tree, view, staged state, and identity, but
backed by the same shared content. The model gives Lore the equivalent of multiple worktrees
per repository, but without the main-repository / linked-worktrees asymmetry that other
systems impose.

#### 24.1 Shared store and instances

The *shared store* holds the bytes: cached fragments, the mutable store, and the deployment
configuration. Multiple instances of the same repository can be configured to use a single
shared store, which means they share fragment storage and benefit from each other's caches.
Pulling a fragment for one instance makes it instantly available to another. Storage cost is
paid once per fragment, regardless of how many instances are using it.

An *instance* is a working directory with its own state. Each instance has a unique identity:
a UUIDv7 stored in `.lore/instance` at the working directory's root. The instance ID is what
distinguishes one instance's state from another when several instances share a store.

Per-instance state lives in the local mutable store under instance-keyed entries:

- The instance's *current revision anchor* - which revision is materialized in this working
  directory.
- The instance's *current branch* - which branch this working directory is operating on.
- The instance's *staged anchor* - a pointer to the state tree recording the instance's dirty
  and staged divergence from the committed revision.
- The instance's *metadata blob* - the path on disk, creation time, and other information for
  diagnostics and stale-instance detection.

These per-instance entries are derived from the instance ID, so two instances over the same
shared store have completely independent working state without interfering with each other.

#### 24.2 Worktree-like, but no main repository

Other version-control systems offer multiple working directories via a main repository plus
auxiliary "worktrees" or "linked worktrees" that depend on it. Removing the main repository
invalidates the linked worktrees; cloning an auxiliary worktree fetches its content from the
main; the main is privileged in ways that the others are not.

Lore does not have this asymmetry. The shared store is owned by no instance in particular;
all instances over it are peers. Removing any one instance's working directory leaves the
others intact and unaffected. There is no "main instance" to maintain. New instances can be
added or removed at any time, and removing the last instance over a shared store does not
remove the store itself - the store persists as long as something is configured to use it.

This matters because the main / linked split forces an ordering on operations: the main has
to exist first, the auxiliary instances are derived from it, and tearing down the main is
disruptive. Lore's symmetric model lets a workflow create and destroy instances freely,
without bookkeeping about which one is "the" repository.

#### 24.3 Use cases

The shared-store model enables several patterns that don't fit cleanly elsewhere:

- *Parallel branch builds*. CI infrastructure can spin up an instance per branch, each
  building its own version of the working tree, all backed by one fragment store. Storage
  cost grows with unique content, not with the number of build agents.
- *Agent-based development*. Tools that run AI agents or other automated workers on multiple
  branches concurrently can give each agent its own instance. The agents work in isolation
  but share fragment cache and mutable-store state where they overlap.
- *A/B development*. A developer comparing two branches in parallel can keep two instances
  open without re-cloning the repository or paying twice the storage cost.
- *Per-tool instances*. Different tools that want different sparse views of the same
  repository can each maintain their own instance with its own view, all over the same store.

In each case, the property that matters is symmetry: instances are equal peers, and the
shared store is a substrate, not a hierarchy.

### 25. Comparison with Prior Art

§2 framed Lore against Git, Perforce, and Mercurial/Sapling from the angle of what
they do well and where they fall short of Lore's target workloads. Now that the rest
of the document has described Lore in detail, this section gives the head-to-head
version: where Lore shares concepts with each system, where it diverges, and a
closing synthesis of what Lore is willing to do differently.

#### 25.1 Lore vs. Git

Git is the closest existing system to Lore. They share content addressing, a Merkle
DAG of revisions, and per-content hashing as the foundation. They differ on almost
every other axis.

*Concepts shared with Git.* The fundamental shape - hash-addressed content in a tree,
revisions with parent hashes, branches as named pointers, three-way merge - is common
to both. Lore did not invent any of this; Git proved the design, and Lore treats the
proof as background.

*Where Lore diverges.*

- *Distributed vs centralized.* Git is a true peer-to-peer DVCS; every clone is a
  full participant. Lore is centralized in role: a deployment is the source of truth,
  and clones are sparse, lazy views into it (§14). The asymmetry enables multi-tenant
  safety, partition-scoped access control, and storage tiering at scale.
- *Full clone vs sparse default.* A Git clone fetches the full repository history
  and tree by default. A Lore clone fetches only what the user's view requests, with
  the rest fetched lazily (§12). Git's modern partial-clone + sparse-checkout
  combination approaches this but is opt-in and has sharp edges (§2.3).
- *Object format vs fragment + chunking.* Git stores each file as one object; large
  files are LFS-bolted-on. Lore chunks files into fragments (§8), addressed
  individually, with cross-file dedup at the fragment level. A multi-gigabyte binary
  edited by one byte re-uploads only the changed chunks.
- *Object wrapping vs raw bytes.* Git's object hash is computed over the bytes
  prefixed with a Git-specific header (`blob <size>\0`, `tree <size>\0`, etc.), so the
  SHA-1 of a Git blob is not the SHA-1 of the file's raw content - it depends on
  Git's wrapping. Lore's fragment hash is computed over the raw payload bytes alone;
  metadata (size, flags, compression) lives in a separate fragment header that is not
  part of the hashed content. Anyone with the raw bytes of a file can derive its Lore
  address without knowing Lore-specific framing; an external tool that hashes the
  file with `b3sum` and a Lore client looking up the same file arrive at the same
  address.
- *Refs vs mutable store.* Git keeps branch heads in a `refs/` directory under a
  loose convention. Lore keeps branch latest pointers in a typed mutable store with
  conditional-put atomicity (§9). Two clients pushing the same branch in Lore have a
  precise serialization point.
- *API-first vs CLI-first ecosystem.* Git was built CLI-first; integrations either
  parse CLI output (which is unstable across versions) or depend on libgit2 - a
  separate project that re-implements Git's core as a permissively-licensed C
  library, because Git itself is GPL-2.0 and was not designed to be linked. libgit2
  is widely used (GitHub, GitLab, Azure DevOps) but lags Git's own feature
  development, since the two are different codebases evolving on different schedules.
  Lore is API-first (§6): the C library and language bindings are the primary
  artifact, the CLI is a thin layer over the same API, and there is no parallel
  library project to keep in sync with the canonical implementation.
- *Server model.* A Git server is a wire-protocol endpoint over a generic transport -
  git-daemon, SSH, or HTTPS - and the server-side typically runs the same git
  executable as the client, with the hosting platform wrapping it. The protocol
  itself carries packed objects and reference updates over a stream; structured
  commands, sessions, and authorization scoping live in whatever platform sits in
  front of it. Lore's server is a dedicated server process speaking typed binary
  protocols (QUIC and gRPC, §18) with structured commands, server-managed sessions,
  and JWT-scoped authorization built into the protocol. Multi-tenancy, validation,
  and policy enforcement are inside the server, not bolted on by an outer platform.
- *Multi-tenancy.* Git itself has no multi-tenant security model; tenancy is enforced
  by adjacent infrastructure. Lore's partition-based access boundary (§17) is part
  of the system.
- *Access control granularity.* Git has no native per-path or per-branch access
  control. A naked Git server enforces nothing past "you can reach this repository
  or you cannot"; per-branch and per-path policies live in hooks and in wrapping
  platforms (GitHub teams, GitLab protected branches, Gitea permissions) that
  enforce them around the wire protocol. Lore has per-repository access control at
  the storage layer - each repository is one partition (§17) - and per-directory
  access control via link composition (§5.6): any directory needing its own access
  policy is elevated to a separate repository linked back into the parent. The ACL
  is checked by the server itself on every operation.

#### 25.2 Lore vs. Perforce

Perforce is the prior art Lore most resembles in *role*: a centralized
server-of-record holding the canonical state of a repository that may contain very
large files, with file-level locking and a clear administrative boundary. Lore takes
Perforce's role seriously and rebuilds the substrate underneath.

*Concepts shared with Perforce.*

- *Server as source of truth.* The model where canonical state lives at the server
  rather than at every clone is Perforce's contribution to the design space.
- *File-level locking for unmergeable content.* Lore's locking model (§13.4)
  recognizes that some content cannot be merged automatically - the same insight
  Perforce has had for decades.
- *Per-path access control.* Perforce's protections table is a per-path ACL. Lore
  arrives at the same outcome via a different mechanism - directories that need their
  own access policy are linked sub-repositories with their own partition (§5.6) - but
  the operational result is similar.

*Where Lore diverges.*

- *Online by default vs offline-capable.* Perforce requires server round-trips for
  most operations (open for edit, list changes, sync). Lore's normal client
  operations - staging, committing, branching, switching, diffing - work entirely
  locally without contacting the remote (§14).
- *Delta storage vs content addressing.* Perforce's storage is delta-encoded RCS for
  text and full-file for binaries. Lore's storage is content-addressed fragments with
  cross-file dedup (§7, §8); two files that share content share storage
  automatically.
- *MD5 vs BLAKE3.* Perforce uses MD5 for integrity. Lore uses BLAKE3, a modern
  cryptographic hash (§7.2). The difference matters for integrity guarantees in
  adversarial settings.
- *Closed system vs open source and open spec.* Perforce is closed end to end: the
  server is closed-source, the wire protocol is proprietary, the on-disk storage
  formats are proprietary, and there is no third-party server implementation. Lore is
  MIT-licensed end to end (§3.1) - client, server, language bindings, storage backends
  - and every data format and wire protocol is publicly specified and versioned
  (§6.4). Anyone can read, implement, or audit the formats; anyone can build a
  server, a client, or a tool against the spec without permission.
- *Reconciliation vs filesystem-as-truth.* Perforce requires `p4 reconcile` to make
  out-of-band file changes visible. Lore reads the filesystem directly (§15.1). A
  developer can use any tool to edit files, and Lore picks up the changes the moment
  it walks the working tree.

#### 25.3 Lore vs. Mercurial and Sapling

Mercurial - and Meta's Sapling, which forked from Mercurial and pushed it much
further - solved the scale of source-shaped repositories elegantly. Lore takes several
specific ideas from this lineage and parts company on others.

*Concepts shared with Mercurial / Sapling.*

- *Sparse and lazy data fetching at scale.* Sapling's segmented changelog and lazy
  history loading proved that monorepo-scale workflows could be made fast without
  full clones. Lore's sparse-by-default model (§12) shares this orientation.
- *UX assumptions.* Smartlog and commit-stack workflows in Sapling shaped the
  expectation that a modern VCS does not require its users to think about packfiles,
  reflogs, or detached states. Lore inherits this expectation.

*Where Lore diverges.*

- *Text-first vs binary-first.* Mercurial and Sapling are designed around textual
  histories: line-oriented merges, evolve workflows, text-shaped diffs are
  first-class. Lore is binary-first (§13); text features are layered on top of an
  opaque-bytes substrate.
- *Single-tenant vs multi-tenant.* Sapling is built for one large monorepo at Meta.
  Lore is built for many repositories sharing one backend, with partition-scoped
  isolation (§17) as a load-bearing requirement.
- *Per-file granularity vs fragment-level granularity.* Mercurial's revlog stores
  per-file delta chains; Sapling moved towards content addressing internally but
  retains the file-as-unit assumption. Lore addresses individual fragments below the
  file level (§8) and dedups across files at that granularity.

#### 25.4 What we keep, what we reject, and why

Lore is not novel where it doesn't have to be. The shapes that turned out right in
prior systems - content addressing, Merkle DAGs of revisions, branches as named
pointers, three-way merge for text, server-of-record for canonical state, file-level
locking for unmergeable content, sparse and lazy data fetching - are carried over
unchanged where possible.

The things Lore rejected are load-bearing for the workloads it targets:

- *Git's distributed-by-default model.* Rejected because the workloads Lore targets
  need a single source of truth for access control, durability, and audit, and must
  scale far beyond what every-clone-is-a-full-replica permits.
- *Git's object-as-file granularity.* Rejected because multi-gigabyte binaries do
  not fit the model. Fragment-level addressing solves the same identity problem at a
  finer granularity.
- *Perforce's online-required workflow.* Rejected because developers should be able
  to keep working without network access.
- *Perforce's delta-encoded storage.* Rejected because content addressing supplies
  dedup at fragment granularity without per-file delta machinery.
- *Mercurial / Sapling's text-first orientation.* Rejected because the workloads
  Lore targets are not text-shaped.

What's left after those decisions is a centralized, binary-first, multi-tenant,
sparsely-cloned VCS with content-addressed storage and a public spec. None of those
properties is novel by itself. The point is that no other system commits to all of
them at once.

### 26. Open Problems and Future Work

The design described so far is the system as it stands. Several extensions are intended but not
yet implemented; this section names them, explains the problem each solves, and sketches the
shape of the proposed mechanism. Each is an open problem in the sense that the data model can
already accommodate it but the API surface, tooling, or supporting protocol work has not landed.

#### 26.1 Forking via copy-on-read

A fork in Lore is a separate partition with its own access control, distinct from a branch.
The fork shares its initial content with the source repository but evolves independently from
that point forward; users with access to the fork need not have access to the source, and
changes on either side stay isolated. The use case is the standard one: an external contributor
or a downstream team takes a copy of a repository, develops in isolation, and may eventually
propose changes back upstream.

The challenge under partition-scoped ownership is storage cost on creation. Every fragment in
the source partition would, in the naive model, need to be registered in the fork partition up
front - even though the underlying storage layer already holds the bytes once. On a
multi-terabyte repository this is prohibitive. The desired property is that fork creation is
fast and storage cost is bounded by what the fork actually touches.

The proposed mechanism is *copy-on-read* across the partition boundary. When a session bound
to the fork partition requests a fragment that is not yet registered in the fork, the server
reads through to the source partition (where the fragment is registered), registers it in the
fork partition by deduplication copy (the bytes already exist in the storage layer's underlying
blob store and are not transferred again), and serves the response. After the first read, the
fork partition holds the registration and subsequent accesses go directly through the fork
without read-through. The fork therefore starts empty of registered fragments, fills in lazily
as the user actually touches content, and reaches a steady state proportional to the working
set the fork has exercised - not to the size of the source repository.

Access control is preserved by gating read-through on the session's authorization for both
partitions, the same way Copy works today (§17.6); the source partition's content does not
leak just because the fork exists. Open questions remain on how source-partition obliteration
interacts with fork content that has not yet been read through, on quota accounting (does the
fork accumulate quota on registrations created by read-through, or only on writes the fork
itself originates?), and on sync-back semantics when both sides evolve and the user wants to
merge fork changes upstream.

#### 26.2 Branch-aware and policy-driven locking

Today's locks are repository-wide and file-level (§13.4): a lock prevents anyone other than
the holder from modifying the file regardless of branch, and a release returns the file to
unlocked globally. This works for the simple case of a single asset edited by one person at a
time, but interacts badly with branch-based workflows.

The interaction problem is that even when a lock is held globally, the *result* of a lock - the
modification made while the lock was held - lives only on the branch where it was committed.
A user on branch A acquires a global lock, edits the asset, commits, and releases. The release
is fine; the asset is globally unlocked. A user on branch B then acquires a global lock, edits
the asset, commits, and releases. Both locks were correctly serialized in time, and neither
user violated the locking contract, but the modifications happened in parallel branches that
have not yet been merged. When branch A eventually merges into branch B's line, the changes
collide. Locks alone are not enough: locking is temporal (who is editing now), but the result
of a lock is persistent and lives on a specific branch.

Branch-aware locking is the fix. A lock acquisition request must check not only "is this file
locked right now" but also "has the result of every previous lock on this file already merged
into the branch the requestor is working on." If yes, the lock can be granted; the requestor's
branch already contains all prior modifications and a new modification cannot race with them.
If no, the lock acquisition is either refused (forcing the requestor to sync first) or
qualified as branch-scoped, allowing parallel locks on disjoint branches but surfacing the
conflict explicitly when those branches merge.

Policy-driven acquisition layers on top of the branch-aware mechanics: lock behavior can vary
by configured policy - auto-locking files matching a pattern (binary asset directories), lease
expiry to release stale locks, approval workflows for sensitive content, audit trails of who
held what when. A locked file that moves through merge, rebase, or cherry-pick has to follow
the operation correctly: the result of the operation in the new branch is still the result of
the original lock, and the locking semantics must account for that without forcing manual
reacquisition.

#### 26.3 Multiple concurrent stages

The working tree holds a single stage today: one set of staged paths waiting to be committed
to the next revision. The stage is recorded as intent in the local mutable store (§15.2);
committing turns the stage into a revision and clears it.

Multi-stage support would let a single working tree carry several disjoint stages in flight
at once, each committable independently. A given file can be in at most one stage at a time -
no ambiguity about which stage owns the change - but two stages with disjoint file sets
coexist over the same working tree without interference. The pattern is the one Perforce calls
a *changelist*: a developer can have a bug fix and a feature in flight simultaneously, each
with its own set of staged paths and its own revision commit message, and ship them as separate
revisions in either order.

The use cases the model fits include a bug fix that should ship quickly while a feature is
still being polished, a pair of independent refactors in the same area of the tree that should
land as separate revisions, and an emergency hotfix that needs to go out without picking up
half-finished work in the working directory. The data model already supports it - each stage
is a list of paths in the local mutable store, and a commit reads a specific stage's path
list. What is missing is the API surface and the tooling: naming stages, switching the current
stage, listing stages, surfacing the right stage in `status`, and integrating with the rest of
the version-control workflow.

#### 26.4 Hash-based server sharding

A Lore deployment today is one logical fleet behind one logical endpoint. Capacity grows by
adding servers and replicas, all of which speak for the same logical backend. There is no
native partitioning of the backend itself by content - every server in the fleet can answer
about every fragment.

Hash-based sharding would route reads and writes to a specific server or cluster based on the
*tail* bytes of the fragment hash. Within each server, local on-disk distribution continues to
use the *front* bytes of the hash (the existing bucketed fan-out, §20.1). The two axes are
orthogonal and stack cleanly: front-of-hash gives uniform on-disk fan-out within a server;
tail-of-hash gives uniform fan-out across servers within a deployment.

Two properties follow. First, the same hash always routes to the same shard, so deduplication
is preserved across the deployment without coordination between shards - the fragment lives
once globally, on whichever shard its hash routes to. Second, read and write capacity scales
near-linearly with shard count, since fragment traffic distributes uniformly by hash and no
single shard becomes a hot spot for content the way a single-fleet deployment can.

Open questions remain on where the routing layer lives - transparent to clients with the edge
tier doing the routing, or visible in the protocol so clients can talk directly to shards;
on rebalancing behavior when a shard is added or removed (which fragments re-route, and how
is in-flight traffic migrated); and on cross-shard operations such as a push that references
fragments touching multiple shards. The mutable store does not shard the same way - latest
pointer advances are partition-scoped, not hash-scoped - so the sharding strategy for the
mutable store is a separate question.

### 27. References

This document is self-contained; nothing in the body depends on any external citation
to be understood. The references below are the substantive prior work and adjacent
systems that informed Lore's design - useful as further reading, not as required
context.

#### 27.1 Algorithms and primitives

- **FastCDC: a Fast and Efficient Content-Defined Chunking Approach for Data
  Deduplication.** Wen Xia, Yukun Zhou, Hong Jiang, Dan Feng, Yu Hua, Yuchong Hu,
  Yucheng Zhang, and Qing Liu. USENIX ATC '16, 2016.
  <https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf>
- **BLAKE3: One Function, Fast Everywhere.** Jack O'Connor, Jean-Philippe Aumasson,
  Samuel Neves, and Zooko Wilcox-O'Hearn. The BLAKE3 specification and reference
  implementation. <https://github.com/BLAKE3-team/BLAKE3-specs>
- **A Digital Signature Based on a Conventional Encryption Function.** Ralph C. Merkle.
  CRYPTO '87. The original paper introducing the hash-tree construction now widely
  called a Merkle tree.
- **Zstandard.** Yann Collet. RFC 8478, "Zstandard Compression and the
  application/zstd Media Type." <https://datatracker.ietf.org/doc/html/rfc8478>
- **UUID Version 7.** Kyzer R. Davis, Brad G. Peabody, and Paul J. Leach. RFC 9562,
  "Universally Unique IDentifiers (UUIDs)," section on version 7.
  <https://datatracker.ietf.org/doc/html/rfc9562>

#### 27.2 Transports and protocols

- **QUIC.** Jana Iyengar and Martin Thomson, eds. RFC 9000, "QUIC: A UDP-Based
  Multiplexed and Secure Transport." <https://datatracker.ietf.org/doc/html/rfc9000>
- **gRPC.** <https://grpc.io/> - the framework Lore uses for the gRPC variant of the
  storage protocol.
- **JSON Web Token (JWT).** Michael B. Jones, John Bradley, and Nat Sakimura. RFC
  7519. <https://datatracker.ietf.org/doc/html/rfc7519>

#### 27.3 Comparable systems

- **Git.** <https://git-scm.com/>. The dominant distributed VCS, the closest existing
  analogue for Lore's content-addressed revision graph (§25.1).
- **libgit2.** <https://libgit2.org/>. A separate library re-implementing Git's core
  for embedding into applications.
- **Mercurial.** <https://www.mercurial-scm.org/>. The Python-based DVCS whose
  scaling work informed Sapling.
- **Sapling.** <https://sapling-scm.com/>. Meta's source-control client and the
  source of the segmented changelog and lazy history loading approach (§25.3).
- **Perforce / Helix Core.** <https://www.perforce.com/products/helix-core>. The
  canonical centralized VCS for large-content workflows; the prior art for Lore's
  centralized server-of-record model (§25.2).
- **Plastic SCM / Unity Version Control.**
  <https://www.plasticscm.com/>. Centralized + distributed hybrid VCS aimed at
  game-development workflows.
- **Pijul.** <https://pijul.org/>. Theory-of-patches DVCS; an interesting comparison
  point for revision-as-graph models even though Lore does not adopt patch theory.
