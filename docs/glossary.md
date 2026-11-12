# Lore glossary

Definitions for the Lore-specific vocabulary used in the documentation and the `lore` CLI. Entries are alphabetical, case-insensitive. The first occurrence of a term on a doc page should link here; subsequent occurrences on the same page don't.

For the rules used to maintain this file, see [`developing/doc-standards/operational/glossary-conventions.md`](developing/doc-standards/operational/glossary-conventions.md).

## A

### Abort

To stop merging, cherry-picking, or reverting partway through and discard the conflict state (`lore branch merge abort`, `lore revision cherry-pick abort`, `lore revision revert abort`).

Compare to: resolve.

Also see: conflict.

### Access token

A token that grants access to a repository, normally obtained by exchanging an identity token with the authentication service. It's a JWT that carries the user's permissions and an expiry time.

Compare to: identity token.

Also see: authentication service, JWT.

### Address

The pair (hash, context) that identifies a fragment in the immutable store: 32 bytes of BLAKE3 hash plus 16 bytes of context, 48 bytes total. Two fragments with the same address are the same fragment.

Also see: hash, context, fragment.

### Amend

To replace a branch's latest revision with a new revision that has the same content and a different message (`lore revision amend`). The new revision gets a new hash signature, and the original stays in the store.

Compare to: commit.

### Anchor

An instance's local record of the branch and revision it's on, and of its staged changes. It never leaves the machine.

Also see: bare, repository instance.

### Archive

To retire a branch so it no longer appears in branch listings and its name can be reused, keeping its history (`lore branch archive`). Archived branches can still be listed with `lore branch list --archived`.

Also see: branch.

### Authentication service

The service that authenticates users and issues the tokens Lore uses to prove identity and authorize access to repositories. A repository's remote names it through an auth URL, such as `ucs-auth://auth.example.com`.

Also see: identity token, access token.

### Auto-follow

The behavior by which a link or layer creates or switches the matching branch in its target repository when the parent repository does. It's on by default.

Compare to: link, layer.

## B

### Bare

A way of cloning or switching branches that records the revision without writing or checking any working files (`--bare`). The instance's anchor moves to the revision, but no files on disk are written or changed.

Also see: anchor, working tree.

### Base revision

The nearest common ancestor of two branches' latest revisions, from which a branch diff or a three-way merge operation calculates the changes on each side.

Also see: three-way merge, branch point.

### Binary-first

Lore's design principle that all content is handled as opaque byte streams on the storage and transport paths — no line-ending translation, encoding inference, or clean/smudge filters. Text-aware behavior is layered on top, never assumed below.

### Bisect

To binary-search a range of revisions for the one that introduced a change (`lore revision bisect`). The start revision is the latest known not to have the change, exclusive; the end revision is the earliest known to have it, inclusive.

### BLAKE3

The 256-bit cryptographic hash function Lore uses as its content address function, chosen for cryptographic strength, throughput on commodity CPUs, and parallel hashing of large inputs.

For more information, see:
BLAKE3 <https://github.com/BLAKE3-team/BLAKE3-specs>

### Branch

A named, movable pointer to its latest revision. A branch has a stable ID, so its name can be reused without losing history.

Also see: branch ID, latest, revision graph.

### Branch ID

A 16-byte opaque identifier, written in hexadecimal, that identifies a branch independently of its name. It's generated when a branch is created, or set explicitly with `lore branch create --id`.

Also see: branch.

### Branch point

The revision a branch was created at, and the last one it shares with the branch it was created from. A branch point identifies the branch it was created on, and can also identify the child branch by naming that child branch.

Also see: branch, revision identifier.

## C

### Case handling

The policy for staging a path whose name changed only in letter case (`lore stage --case`). It decides whether Lore stops, keeps the repository's case, or takes the file system's.

Also see: stage.

### CDC

See content-defined chunking.

### Change request

A change request (CR) is a proposed set of revisions submitted for review before merging. Reviewers comment on it and approve it, and it's then merged into its target branch.

### Cherry-pick

To apply one revision's changes on top of the current revision, creating a new revision (`lore revision cherry-pick`). The original revision is unchanged, and conflicts are resolved the same way as when merging.

Compare to: revert.

Also see: conflict.

### Chunking

Splitting a file into a sequence of smaller fragments, each addressed independently, so an edit re-stores only the affected fragments and any byte range can be read without materializing the whole file. Lore supports content-defined and fixed-size chunking.

Also see: fragment, content-defined chunking, fixed-size chunking.

### CLI

A command-line interface (CLI) is the text-based way to drive Lore, through the `lore` binary and its subcommands.

### Clone

To create a new repository instance, either from a repository on a remote (`lore clone`) or from another instance on the machine (`lore repository instance clone`). What cloning creates is an instance.

Compare to: repository instance.

### Commit

To record the staged changes as a new revision (`lore commit`). The new revision becomes the branch's local latest; pushing sends it to the remote.

Compare to: revision.

### Compaction

The background process that keeps the local store under its maximum size by removing the least recently used content and rewriting its packfiles. Like eviction, it removes only content the remote also holds.

Also see: eviction, garbage collection.

### Conflict

A path that both sides changed in ways Lore can't combine automatically when merging, cherry-picking, or reverting, left for the user to resolve. A path can be resolved, marked unresolved again, or restarted, and the whole operation can be aborted.

Also see: resolve, unresolve, abort, three-way merge.

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

### Dependency

A recorded edge from a source file to a file it depends on, optionally labeled with tags (`lore file dependency add`). Lore uses dependency edges to clone or sync only chosen files and the files they need.

Also see: dependent, dependency tag, root file, selective sync.

### Dependency tag

A label on a dependency edge. A tag filters which edges are listed, removed, or followed when cloning or syncing by dependency.

Also see: dependency.

### Dependent

A file that depends on a given file. `lore file dependency list --reverse` lists dependents instead of dependencies.

Also see: dependency.

### Diff

A comparison that reports what changed between two revisions, two branches, or a revision and the working tree (`lore diff`, `lore revision diff`, `lore branch diff`). A file diff is a two-way unified diff by default, or a three-way output with conflict markers with `--diff3`.

Also see: base revision, three-way merge.

### Direct write

A setting that writes files in place instead of through a temporary file, which is faster but not atomic. An error while writing can leave a file incomplete.

### Dirty

To mark a file or directory as changed — modified, moved, copied, or deleted — so Lore picks up the change without scanning the file system (`lore dirty`). The mark is local and never committed or pushed.

Compare to: stage.

Also see: scan.

## E

### Eviction

The removal of the least recently used content from the local store to keep it under its capacity. Lore evicts only content the remote also holds, and fetches it again when needed.

Also see: compaction, lazy fetch.

## F

### Fast-forward merge

A push option that has the server merge the pushed revision into the branch when the branch's latest has moved (`lore push --fast-forward-merge`). The server merges only if there are no conflicts; otherwise pushing fails.

Compare to: merge.

### FastCDC

The content-defined chunking algorithm Lore uses, placing chunk boundaries with a rolling hash subject to minimum and maximum chunk sizes.

Also see: content-defined chunking.

For more information, see:
FastCDC <https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf>

### Fixed-size chunking

A chunking strategy that places fragment boundaries at fixed byte offsets regardless of content, giving each piece of content exactly one canonical address at the cost of deduplication robustness across insertions.

Compare to: content-defined chunking.

### Fragment

The content-addressed unit of storage in Lore. Files are split into fragments and the repository tracks fragments by hash; identical fragments are deduplicated across the store.

### Fragment reference

An entry in a fragment list recording the hash of a child fragment and the byte offset it represents in the reassembled content; the offset is what makes sparse, range-based reads possible.

Also see: fragment, chunking.

## G

### Garbage collection

The reclaiming of space in the local repository store. An incremental pass runs in the background after writes, unless suppressed with `--no-gc`, and `lore repository gc` always runs a full pass.

Also see: compaction, eviction.

## H

### Hash

A 32-byte BLAKE3 hash of a content payload, Lore's content address throughout. The hash answers what a fragment is; the partition answers who can access it.

Also see: address, BLAKE3, partition.

### Hash signature

The hash that identifies a revision, derived from its serialized state, including its parent revision hashes and the hashes of the data it contains. Its whole 64-character form is accepted as a revision identifier.

Also see: revision, revision identifier.

### Heal

To repair a problem that verify found (`lore repository verify --heal`). A damaged fragment is fetched again from the remote.

Also see: verify.

### History

The ordered list of revisions reachable from a starting revision, newest first, for a repository, branch, or file (`lore history`, `lore file history`).

Also see: revision graph.

## I

### Identity

The user identity Lore records on the revisions committed from a repository. It's set when the repository is created or cloned.

Also see: identity token.

### Identity token

A token that proves who a user is to the authentication service, kept on the machine after login. Lore exchanges it for access tokens to reach repositories.

Compare to: access token.

Also see: authentication service, JWT.

### Ignore file

A list of path patterns for new files that Lore leaves out of staging and committing: `.loreignore` in the repository, and `.lore/ignore` for one instance only. Files already in the repository aren't affected. For the rule syntax, see [Write view filters and ignore files](how-to/write-view-filters-and-ignore-files.md).

Compare to: view filter.

### Immutable store

The content-addressed data store that holds every byte Lore persists, keyed by BLAKE3 hash. It's append-only: entries are added or obliterated, never modified in place.

Compare to: mutable store.

### Inherited metadata

Metadata keys carried from a source revision onto the revision that merging or cherry-picking creates (`--inherit-metadata <key>`, or `*` for every key not reserved to the operation). Nothing is carried unless requested.

Also see: metadata, merge.

### Instance

See repository instance.

## J

### JWT

A JSON Web Token (JWT) — the bearer token Lore uses for authentication and authorization over both its QUIC and gRPC transports, encoding user identity, authorized partitions and permissions, expiry, and a signature.

For more information, see:
JWT <https://datatracker.ietf.org/doc/html/rfc7519>

## L

### Latest

The pointer to a branch's most recent revision. The local and remote latest can differ until the next sync.

Also see: branch, mutable store, sync, reset.

### Layer

A per-machine overlay of part of another repository at a mount path in this one, not recorded in any revision. One of two Lore analogs to a Git submodule.

Compare to: link.

Also see: mount path.

### Lazy fetch

The default model in which a client pulls only the fragments it needs, on demand, leaving the rest on the remote.

Compare to: prefetch.

Also see: sparse working tree, fragment.

### Link

A reference from one repository to a revision of another, mounted at a path and recorded in the parent's revisions, so every instance cloned from them has it. The other Lore analog to a Git submodule.

Compare to: layer.

Also see: mount path, pin, partition.

### Lock (n)

An exclusive claim on a file on a branch, used to coordinate edits to content that can't be merged, such as binary assets. Locks are advisory: Lore doesn't stop anyone from committing or pushing changes to a locked file.

Also see: binary-first.

### Lock (v)

To claim a file on a branch so others know not to edit it until it's unlocked (`lore lock`). Locking fails if another user already holds a lock on the same file.

### Lore

The product, the system, the project, and the open-source project. Capital `L` when referring to any of these. Lowercase `lore` (in a code span) refers to the CLI binary or a command.

### Lore directory

The `.lore/` folder in a repository instance that holds the instance's local state: its instance ID, repository config, view filter, ignore file, modified-time records, and, unless a shared store is used, its local stores.

Also see: repository instance, view filter.

## M

### Machine server

A Lore server on a build machine that caches content for the containers on that machine and passes their requests on to the upstream. Containers on the machine share one cache instead of each fetching from the remote.

Also see: upstream, remote.

### Merge

To combine two separate lines of history into one new merged revision (`lore branch merge`). If both sides changed the same content in incompatible ways, merging stops so the user can resolve the conflicts.

Also see: conflict, three-way merge.

### Merkle tree

A tree in which each node's hash derives from its children's hashes, so a single root hash identifies and verifies the whole structure. A Lore revision's files and directories are a Merkle tree, which makes structural deduplication and tamper-evidence automatic.

For more information, see:
Merkle tree <https://en.wikipedia.org/wiki/Merkle_tree>

### Metadata

Typed key-value data attached to a file, revision, branch, or repository, used by Lore and by applications. Revision and file metadata are part of history; branch and repository metadata can change.

Also see: immutable store, mutable store, inherited metadata.

### Mine

In conflict resolution, the local side: the changes already on the current branch. Resolving a path as mine keeps them (`resolve mine`).

Compare to: theirs.

Also see: conflict, resolve.

### Modified-time record

The time at which a file of an instance last held its revision's content, kept so a scan can skip hashing a file whose time has not moved: in the instance's Lore directory, or in the mutable store for an SWFS or in-memory instance. Cloning an instance carries over its source's records.

Also see: repository instance, scan.

### Mount path

The path in a repository at which a link or layer is placed. Commands that act on a single link or layer identify it by its mount path (`--link`, `--layer`).

Also see: link, layer.

### Multi-tenancy

Hosting many unrelated repositories on one backend deployment without cross-tenant content leakage, even when tenants can guess each other's content hashes. It's enforced by partition-scoped access.

Also see: partition.

### Mutable store

The small key-value store holding state that content addressing can't express — branch latest pointers, name-to-ID mappings, repository catalogue entries. It's the only place in Lore where concurrent writers serialize, through the conditional-put primitive.

Compare to: immutable store.

## N

### Notification

An event the remote publishes about a repository, such as a branch push or a lock, that clients can subscribe to (`lore notification subscribe`). Tools can use them to react to changes without polling the remote.

Also see: remote.

## O

### Obliteration

The permanent removal of stored content, while the revisions that reference it stay valid (`lore file obliterate`). Reading obliterated content finds nothing, and other files that share the same bytes are unaffected.

Also see: context, address.

### Offline mode

A mode in which commands work only with what's on the machine, without contacting the remote (`--offline`). Commands that can't work without the remote fail instead of trying to connect.

Also see: remote URL.

## P

### Partition

A 16-byte opaque identifier that scopes content in the storage subsystem and is Lore's access boundary: every fragment and mutable-store key belongs to exactly one partition, and a session is authorized per partition. Each repository corresponds to one partition.

Also see: multi-tenancy, context.

### Pin

The branch or specific revision a link tracks in its target repository. A pin is set when the link is added and changed with `lore link update`.

Also see: link.

### Prefetch

Fetching a listed set of files while cloning, ahead of demand (`lore clone --prefetch <file>`).

Compare to: lazy fetch.

### Protected branch

A branch that only users with the push-protected permission can push to (`lore branch protect`). Other branches need only the write permission.

Also see: branch, push.

### Purge

An option that also deletes files Lore doesn't track when resetting paths or removing a layer (`--purge`). Without it, such files are left in place.

Also see: reset, layer.

### Push

To send local revisions on a branch to the remote (`lore push`, `lore branch push`). Without a branch argument, the current branch is pushed.

Compare to: sync.

Also see: remote, protected branch.

## R

### Remote

The collective server-side environment for a repository — one or more Lore server processes with optional cache, replica, and storage tiers, operated as a unit and serving as the source of truth. The terms remote, backend, and deployment are used interchangeably.

Also see: sync, remote URL.

### Remote URL

The URL that locates a repository on a remote, such as `lore://127.0.0.1:41337/my-project`. It's written to the repository's `config.toml` as `remote_url` when the repository is created or cloned, and is treated as fixed afterwards.

Also see: remote.

### Repository

The versioned collection of a project's files and directories, together with its revisions, branches, and metadata, identified by a name or ID on a remote. Each repository corresponds to one partition.

Also see: partition, repository instance, remote.

### Repository instance

A local working directory of a repository, with its own working tree, view filter and staged changes, identified by an ID in `.lore/instance`. Several instances can share one shared store, so content is fetched and stored only once. The closest Git analog is a worktree, but every instance is an equal peer; see [Run parallel instances over a shared store](how-to/run-parallel-instances-over-a-shared-store.md).

Also see: shared store, working tree, Lore directory.

### Reset

To return something to an earlier state, discarding what changed since. Resetting files discards their local changes and returns them to a revision's content, by default the current revision's (`lore reset`). Resetting a branch moves its local latest to another revision without creating a new revision (`lore branch reset`).

Compare to: unstage.

Also see: purge, latest.

### Resolve

To mark a conflicted path as settled, either after editing it or by taking one side with `resolve mine` or `resolve theirs` (`lore branch merge resolve`). Staging the file alone doesn't resolve it.

Compare to: unresolve.

Also see: conflict, mine, theirs.

### Restore

To make an earlier revision's content the branch's newest state by committing it as a new revision (`lore revision restore`). History is kept: the new revision's parent is the branch's latest.

Compare to: revert, reset.

### Revert

To undo a revision's changes by applying their inverse to the current revision (`lore revision revert`). Conflicts are resolved the same way as when merging or cherry-picking.

Compare to: cherry-pick, restore.

Also see: conflict.

### Revision

A frozen snapshot of the whole repository tree, identified by its hash signature. Each revision has one parent, or two when two lines of history were merged into it. Committing creates a revision.

Compare to: commit.

Also see: revision graph, merge, hash signature.

### Revision graph

The directed acyclic graph (DAG) of revisions, in which every edge is a cryptographic parent link. Branches are named pointers into this graph; merging and cherry-picking produce new nodes in it.

Also see: revision, branch.

### Revision identifier

A string that names a revision to a command: its full hash signature, or a branch followed by `@` and a revision number, `LATEST`, or a hash, such as `main@5` or `main@LATEST`. Leaving out the branch means the current branch, and a `~<count>` suffix steps back that many parent revisions.

Also see: revision, branch, latest, revision number, hash signature.

### Revision number

A sequential number that identifies a revision on a branch, used in the `[branch]@<number>` form of a revision identifier. `lore revision find number` looks a revision up by number.

Also see: revision identifier.

### Root file

A file named as the starting point when cloning or syncing by dependency (`--root-file`). Lore transfers the root files and, following their dependencies, the files they need.

Also see: dependency, selective sync.

## S

### Scan

A walk of the file system under given paths that reconciles each file against the current revision, setting dirty flags on detected modifications, adds, and deletes and clearing stale ones (`lore status --scan`, `lore stage --scan`). Without a scan, Lore reports only files already marked dirty.

Also see: dirty, stage.

### Selective sync

Cloning or syncing limited to chosen root files and the files they depend on (`--root-file`). Dependency tags and a depth limit narrow which dependencies are followed.

Also see: dependency, root file, sparse working tree.

### Service process

A long-running `lore` process that carries out CLI commands for the processes that issue them, so one service process handles every command a user runs on a machine (`lore service`). Commands use it automatically once it's configured.

### Shared store

A single on-disk immutable and mutable store referenced by one or more instances on a machine. Instances over the same shared store share fragment storage and cache while keeping independent working trees, views, and staged state. A shared store is backed by a remote, can be registered as the machine's default, and is managed with `lore shared-store`.

Also see: repository instance, immutable store, mutable store.

### Sparse working tree

A working tree that materializes only the subset of the repository the user has asked for, with the rest fetched lazily on demand. Sparseness is Lore's default operating mode, so operation cost tracks the working set, not the repository size.

Also see: view filter, lazy fetch.

### Stage

To mark a file or directory for inclusion in the next committed revision (`lore stage`). Staging records the path, not its content, which is read when committing. Staging a directory includes only the files in it already marked dirty.

Compare to: unstage.

### Staged revision

The pending revision assembled from the staged changes, which `lore commit` records as a new revision. `lore status` reports it together with the dirty files.

Also see: stage, commit.

### SWFS

Epic's Split Write File System (SWFS), a virtual file system that can provide a repository's directory instead of files materialized directly on disk (`--vfs swfs`).

Also see: VFS.

### Sync

To fetch a branch's new remote revisions and merge them with the local latest (`lore sync`). It can also move the instance to any given revision. It's Lore's analog to Git's `pull`.

Compare to: push.

Also see: merge, remote, revision identifier.

## T

### Targets file

A file listing the paths a command acts on, passed with `--targets` instead of naming each path on the command line.

### Theirs

In conflict resolution, the incoming side: the source branch in a merge operation, or the revision being cherry-picked or reverted. Resolving a path as theirs takes those changes (`resolve theirs`).

Compare to: mine.

Also see: conflict, resolve.

### Three-way merge

A merge operation that uses the base revision of the two revisions being merged, preserving changes from both sides where they don't conflict. Lore ships a default three-way text merge algorithm in the version control subsystem; applications can replace it.

Also see: base revision.

For more information, see:
Three-way merge <https://en.wikipedia.org/wiki/Merge_(version_control)>

## U

### Unresolve

To mark a previously resolved path as conflicted again so it can be resolved differently (`lore branch merge unresolve` and the matching cherry-pick and revert commands).

Compare to: resolve.

### Unstage

To remove a path's change from the staged set so it's left out of the next revision (`lore unstage`).

Compare to: stage, reset.

### Upstream

The remote that another Lore server, such as a machine server, reaches through a `remote` store: it reads the content it lacks from there and writes content through to it.

Compare to: remote.

Also see: machine server.

## V

### VCS

A version control system (VCS) is software that records and manages changes to files over time. Lore is a VCS.

### Verify

To check a repository's state, or a single fragment in the local store, for consistency (`lore repository verify`). Problems can be repaired with `--heal`.

Also see: heal, fragment.

### VFS

A virtual file system (VFS) presents a repository's files without writing them all to disk (`--vfs`). Lore can use the system's preferred VFS or a specific one, such as SWFS.

Also see: SWFS.

### View

See view filter.

### View filter

A per-instance list of path patterns (`.lore/view`) that decides which of the repository's files are written to disk. It applies every time files are written, such as when cloning, syncing, or switching branches, and isn't shared through the remote. The closest Git analog is sparse-checkout. For the rule syntax, see [Write view filters and ignore files](how-to/write-view-filters-and-ignore-files.md).

Compare to: ignore file.

Also see: sparse working tree.

## W

### Working tree

The on-disk state of a repository instance — the files as they appear in the contributor's filesystem.
