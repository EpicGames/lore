# Lore glossary

Definitions for the Lore-specific vocabulary used in the documentation and the `lore` CLI. Entries are alphabetical, case-insensitive. The first occurrence of a term on a doc page should link here; subsequent occurrences on the same page don't.

For the rules used to maintain this file, see [`developing/doc-standards/operational/glossary-conventions.md`](developing/doc-standards/operational/glossary-conventions.md).

## A

### Address

The pair (hash, context) that identifies a fragment in the immutable store: 32 bytes of BLAKE3 hash plus 16 bytes of context, 48 bytes total. Two fragments with the same address are the same fragment.

Also see: hash, context, fragment.

### Auto-follow

The behavior by which a link or layer grows or switches a matching branch in its target repository when the parent does, keeping multi-repository workflows in step. It's enabled by default for links and disabled for vendored content.

Compare to: link, layer.

## B

### Binary-first

Lore's design principle that all content is handled as opaque byte streams on the storage and transport paths — no line-ending translation, encoding inference, or clean/smudge filters. Text-aware behavior is layered on top, never assumed below.

### BLAKE3

The 256-bit cryptographic hash function Lore uses as its content address function, chosen for cryptographic strength, throughput on commodity CPUs, and parallel hashing of large inputs.

For more information, see:
BLAKE3 <https://github.com/BLAKE3-team/BLAKE3-specs>

### Branch

A named, mutable pointer to its latest revision in the revision graph — Lore's analog of Git's `HEAD`, but per-branch. A branch has a stable opaque ID separate from its human-readable name, so a name can be archived, restored, or reused without losing history. Branch state lives in the mutable store.

Also see: latest, revision graph.

## C

### CDC

See content-defined chunking.

### Change request

A change request (CR) is a proposed set of revisions submitted for peer review before merging. The Lore equivalent of a pull request in other version control systems.

### Cherry-pick

To replay a single revision (or a contiguous range) onto a different parent, producing a freshly written revision; the original is unchanged.

Compare to: rebase, squash.

### Chunking

Splitting a file into a sequence of smaller fragments, each addressed independently, so an edit re-stores only the affected fragments and any byte range can be read without materializing the whole file. Lore supports content-defined and fixed-size chunking.

Also see: fragment, content-defined chunking, fixed-size chunking.

### CLI

A command-line interface (CLI) is the text-based way to drive Lore, through the `lore` binary and its subcommands.

### Clone

The act of creating a local repository instance from a remote URL (`lore clone`), and the resulting repository instance. Lore-native — not a Git-ism.

Compare to: repository instance.

### Commit

To record the staged changes as a new revision (`lore commit`).

Compare to: revision.

### Content addressing

The model in which content is identified by the BLAKE3 hash of its bytes rather than by name or location. Integrity verification, immutability, and deduplication all follow from it.

Also see: fragment, hash, immutable store.

### Content-defined chunking

A chunking strategy (CDC) that places fragment boundaries where a rolling hash over the content matches a pattern, so an insertion shifts only nearby boundaries and deduplication survives edits. Lore implements CDC with FastCDC.

Compare to: fixed-size chunking.

### Context

A 16-byte opaque tag carried alongside a content hash to track identity — typically a file's stable ID across moves and copies, and the scope for obliteration. It's a deduplication and identity construct, not an access boundary.

Compare to: partition.

Also see: address.

### CR

See change request.

## D

### DAG

A directed acyclic graph (DAG) is a graph with directed edges and no cycles. Lore's revision graph is a DAG.

Also see: revision graph.

### Deduplication

The automatic storage of identical content only once. Because fragments are addressed by the hash of their bytes, files or revisions that share content share fragments; storage cost grows with what's unique.

Also see: content addressing, fragment.

### Dirty

A flag on a working-tree node indicating that the file at that path differs from the committed revision. It's orthogonal to staged — a file can be dirty, staged, or both — and strictly local: never committed or sent to the remote.

Compare to: stage.

## F

### FastCDC

The content-defined chunking algorithm Lore uses, placing chunk boundaries with a rolling hash subject to minimum and maximum chunk sizes.

Also see: content-defined chunking.

For more information, see:
FastCDC <https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf>

### Fixed-size chunking

A chunking strategy that places fragment boundaries at fixed byte offsets regardless of content, giving each piece of content exactly one canonical address at the cost of deduplication robustness across insertions.

Compare to: content-defined chunking.

### Fork

A separate partition with its own access control that shares a source repository's initial content but evolves independently, distinct from a branch. It's a [planned future capability](roadmap.md), filled in lazily through copy-on-read.

Compare to: branch, partition.

### Fragment

The content-addressed unit of storage in Lore. Files are split into fragments and the repository tracks fragments by hash; identical fragments are deduplicated across the store.

### Fragment reference

An entry in a fragment list recording the hash of a child fragment and the byte offset it represents in the reassembled content; the offset is what makes sparse, range-based reads possible.

Also see: fragment, chunking.

## H

### Hash

A 32-byte BLAKE3 hash of a content payload, Lore's content address throughout. The hash answers what a fragment is; the partition answers who can access it.

Also see: address, BLAKE3, partition.

## I

### Ignore file

An outbound filter (`.loreignore`) declaring which paths are excluded from staging and committing. It applies only to new content; a file already in the committed revision keeps flowing through until explicitly removed.

Compare to: view filter.

### Immutable store

The content-addressed data store that holds every byte Lore persists, keyed by BLAKE3 hash. It's append-only: entries are added or obliterated, never modified in place.

Compare to: mutable store.

### Instance

See repository instance.

## J

### JWT

A JSON Web Token (JWT) — the bearer token Lore uses for authentication and authorization over both its QUIC and gRPC transports, encoding user identity, authorized partitions and permissions, expiry, and a signature.

For more information, see:
JWT <https://datatracker.ietf.org/doc/html/rfc7519>

## L

### Latest

The pointer to the most recent revision on a branch, held in the mutable store. Lore's analog to Git's `HEAD`, but per-branch and never a roving pointer that can detach; a client's latest and the remote's may diverge until a sync.

Also see: branch, mutable store, sync.

### Layer

A local overlay of a subset of one repository's content onto another at a path, applied at materialization time and not stored in any revision. It's configured per machine and absent from every clone — two machines on the same revision can layer differently. One of two Lore analogs to a Git submodule.

Compare to: link.

### Lazy fetch

The default model in which a client pulls only the fragments it needs, on demand, leaving the rest on the remote.

Also see: sparse working tree, fragment.

### Link

A reference from one repository to a specific revision and subtree of another, mounted at a path and recorded in the parent's revision — so it travels with every clone of that revision. Each linked repository is its own partition with its own access control, which is how Lore expresses per-directory access policy. The other Lore analog to a Git submodule.

Compare to: layer.

Also see: partition.

### Lock

A server-recorded, exclusive, file-level claim meant to facilitate coordinated edits to content that can't be merged automatically, such as a binary asset or a serialized scene. Acquiring a lock fails if another user already holds one on the same resource, but locks are *advisory*: the storage, push, and merge paths never consult them, so holding a lock does not stop another user from committing or pushing a change to that file — honoring locks is the responsibility of the client application.

Also see: binary-first.

### Lore

The product, the system, the project, and the open-source project. Capital `L` when referring to any of these. Lowercase `lore` (in a code span) refers to the CLI binary or a command.

## M

### Merge revision

A revision with two parents instead of one, recording both parent state hashes; its tree combines the two parent trees. Lore produces a merge revision when reconciling divergent branch history, including by sync.

Also see: revision, sync.

### Merkle tree

A tree in which each node's hash derives from its children's hashes, so a single root hash identifies and verifies the whole structure. A Lore revision's files and directories are a Merkle tree, which makes structural deduplication and tamper-evidence automatic.

For more information, see:
Merkle tree <https://en.wikipedia.org/wiki/Merkle_tree>

### Metadata

A typed key-value blob attached to a file, revision, branch, or repository — the primary extension point for built-in system data (commit message, timestamps) and arbitrary application data. Revision and file metadata are part of immutable history; branch and repository metadata are mutable annotations.

Also see: immutable store, mutable store.

### Multi-tenancy

Hosting many unrelated repositories on one backend deployment without cross-tenant content leakage, even when tenants can guess each other's content hashes. It's enforced by partition-scoped access.

Also see: partition.

### Mutable store

The small key-value store holding state that content addressing can't express — branch latest pointers, name-to-ID mappings, repository catalogue entries. It's the only place in Lore where concurrent writers serialize, through the conditional-put primitive.

Compare to: immutable store.

## O

### Obliteration

The removal of a fragment's payload bytes while its address is preserved in the store's index, so revisions that reference it stay structurally valid. A read of an obliterated fragment returns a typed absence rather than data or corruption; the scope is a file's context, so obliterating one file never disturbs another that shares bytes.

Also see: context, address.

## P

### Partition

A 16-byte opaque identifier that scopes content in the storage subsystem and is Lore's access boundary: every fragment and mutable-store key belongs to exactly one partition, and a session is authorized per partition. Each repository corresponds to one partition.

Also see: multi-tenancy, context.

## R

### Rebase

To replay a chain of revisions onto a different parent, writing fresh revisions and advancing the branch's latest pointer to the final replayed revision; the original chain still exists in the immutable store.

Compare to: cherry-pick, squash.

### Remote

The collective server-side environment for a repository — one or more Lore server processes with optional cache, replica, and storage tiers, operated as a unit and serving as the source of truth. The terms remote, backend, and deployment are used interchangeably.

Also see: sync.

### Repository instance

A local working directory of a repository, with its own working tree, view, and staged state, identified by a UUIDv7 in `.lore/instance`. Multiple instances can share one shared store on a machine while keeping independent state. Created by `lore clone`.

Also see: shared store, working tree.

### Revision

A frozen snapshot of the entire repository tree, identified by the hash of its serialized state and forming a node in the revision graph; each revision has one parent, or two on a merge. Lore-distinctive — not collapsed into "commit": committing changes produces a revision.

Compare to: commit.

Also see: revision graph, merge revision.

### Revision graph

The directed acyclic graph (DAG) of revisions, in which every edge is a cryptographic parent link. Branches are named pointers into this graph; merge, rebase, and cherry-pick produce new nodes in it.

Also see: revision, branch.

## S

### Shared store

A single on-disk immutable and mutable store referenced by one or more instances on a machine. Instances over the same shared store share fragment storage and cache while keeping independent working trees, views, and staged state.

Also see: repository instance, immutable store, mutable store.

### Sparse working tree

A working tree that materializes only the subset of the repository the user has asked for, with the rest fetched lazily on demand. Sparseness is Lore's default operating mode, so operation cost tracks the working set, not the repository size.

Also see: view filter, lazy fetch.

### Squash

To collapse a chain of revisions into one revision whose tree is the chain's final state and whose parent is the revision before the chain. The squashed revision is freshly written; the collapsed chain remains in the store but unreachable from the branch.

Compare to: rebase, cherry-pick.

### Stage (v)

To record the intent to include a file's change in the next revision (`lore stage <path>`). Staging pins the path, not the content: the commit captures whatever the file looks like at commit time, and fragments are produced at commit, not at stage.

### Stage (n)

The set of file paths marked for inclusion in the next revision.

Compare to: dirty.

### Sync

Lore's analog to Git's `pull`: fetch the new remote revisions on a branch and merge them with the local latest, producing a merge revision (`lore sync`).

Also see: merge revision, remote.

## T

### Three-way merge

A merge that uses a common ancestor revision plus the two revisions being merged, preserving changes from both sides where they don't conflict. Lore ships a default three-way text merge in the revision control subsystem; applications can replace it.

For more information, see:
Three-way merge <https://en.wikipedia.org/wiki/Merge_(version_control)>

## V

### VCS

A version control system (VCS) is software that records and manages changes to files over time. Lore is a VCS.

### View filter

A client-side, glob-based inbound filter (`.lore/view`) declaring which paths are materialized to disk. It's applied on every materialization — clone, sync, branch switch, restore — not only at clone time, and local to an instance so it never travels with a clone. Mechanically close to a Perforce stream view but per-instance; closest Git analog: sparse-checkout.

Compare to: ignore file.

Also see: sparse working tree.

## W

### Working tree

The on-disk state of a repository instance — the files as they appear in the contributor's filesystem.
