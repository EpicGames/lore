---
lep: 2026-10-23-low-level-revision-graph-api
title: Low-level revision graph API
authors:
  - mattias.jansson
status: Accepted
created: 2026-10-23
updated: 2026-11-10
discussion: change-request/994
---

# Low-level revision graph API

## Summary

This proposal adds the low-level revision graph API, a new API surface in the `lore_revision_graph_*` namespace. It sits alongside the `lore_revision_tree_*` surface that the [low-level memory-based revision control API](2026-05-14-low-level-revision-api.md) proposal added; this document calls that surface the low-level revision tree API. The revision tree API works on the tree of one [revision](../glossary.md#revision). The revision graph API works on a repository's [revision graph](../glossary.md#revision-graph): its [branches](../glossary.md#branch) and the revisions they point at. A caller needs only a storage handle and a repository id, with no [working tree](../glossary.md#working-tree) and no [repository instance](../glossary.md#repository-instance). It can resolve a revision from a [hash](../glossary.md#hash) or from a branch and revision number, and resolve a branch name to its id. It can list, create, and archive branches, and report where the repository instances over a shared store stand on a branch. It can [sync](../glossary.md#sync) a branch's local [latest](../glossary.md#latest) to the [remote](../glossary.md#remote) latest or to a given revision, and push a branch's local history to the remote. Every verb is stateless. The local and remote flags the storage API already uses decide which locations a call consults. With no flag, a call consults both, remote first, and reports each location's answer in its own record. Sync and push work across both locations by nature: sync always writes the local latest, and push always reads the local history it publishes. The first version ships seven verbs, and the proposal defines the conventions and the amendment process through which later repository-level verbs join the surface. It also changes how the existing branch verbs claim names and restore archived branches, how a client reports a request the remote refuses, and the code a file-system create reports for an invalid name, a zero id or an invalid parent, five of the changes breaking (see Compatibility).

## Motivation

With the low-level revision tree API, a pipeline loads a revision tree from a storage handle, edits it, and [commits](../glossary.md#commit) it. All of this happens in memory, with no working tree. Its accepted proposal limits the surface to "the minimum viable set … load, traverse, edit, commit, close," and says that additional capabilities "fall outside this LEP and land in follow-on LEPs scoped to demonstrated need" ([Non-Goals](2026-05-14-low-level-revision-api.md#non-goals)). Pipelines that use it now need the operations it left out, and every one of them acts on the repository, not on one tree.

**A pipeline can't find out what a branch points at.**

- `lore_revision_tree_load` takes a revision hash and nothing else.
- The only public way to turn a branch into a hash is the file-system API. Its branch and revision verbs select a repository through `globals.repository_path` and load a repository instance from its `.lore` directory (`lore/src/call.rs:74-175, 242-270`).
- The workaround is to read the [mutable store](../glossary.md#mutable-store) directly and re-derive Lore's internal key scheme. A branch's latest sits under a BLAKE3 hash of a salt, a function name, the repository, and the branch (`lore-revision/src/branch.rs:482-496`, salt at `lore-revision/src/repository.rs:1439`). That scheme is an implementation detail, not a published contract.
- A service that serves reads through the revision tree API needs a branch's latest before each read, which puts the workaround on its hottest path.

**A pipeline can't list branches.**

- `lore_branch_list` needs an instance: it runs through the instance call path (`lore/src/call.rs:242-270`), and a local list reads the instance's current branch to report `is_current` (`lore-revision/src/branch.rs:2141-2143`).
- `lore_storage_mutable_list` serves the local store only (`lore/src/storage/mutable_list.rs:87-93`), so it can't enumerate the remote.
- Locally, it returns branch ids keyed by salted name hashes (`lore/src/storage/mutable_list.rs:117-118`, `lore-revision/src/branch.rs:656-661`). Reaching a branch's name from its id means re-deriving another internal key.
- A caller that needs the list today makes a bare [clone](../glossary.md#clone) for it.

**A pipeline can't create or archive a branch.**

- `lore_branch_create` builds the new branch's stack of [branch points](../glossary.md#branch-point) from the instance's current branch and revision (`lore-revision/src/branch/create.rs:104-129`), then switches the instance onto the new branch (`lore-revision/src/branch/create.rs:216-221`).
- `lore_branch_archive` runs through the same instance-bound call path (`lore/src/branch.rs:1159-1179`).

**A pipeline can't publish what it commits.**

- `lore_revision_tree_commit` advances the branch's latest in the storage handle's local mutable store and marks it divergent from the remote (`lore-revision/src/commit.rs:4545-4554`).
- Its `remote_write` option only adds remote writes for data bound for the [immutable store](../glossary.md#immutable-store) (`lore/src/revision_tree/commit.rs:215-222, 251`, `lore-revision/src/immutable.rs:121-128`).
- Lore Server refuses generic mutable-store writes to branch latest pointers and branch [metadata](../glossary.md#metadata) (`lore-server/src/protocol/storage/mutable_store_handler.rs:52-72`, enforced at `:83` and at `lore-server/src/protocol/storage/mutable_cas.rs:68`), so the storage API can't publish a latest either.
- An importer or mirror that builds revisions in memory, the use cases the revision tree proposal names ([Motivation](2026-05-14-low-level-revision-api.md#motivation)), can't get them onto the remote without a clone and `lore_branch_push`.

**A local latest that lags the remote blocks commits.**

- A storage handle backed by a disk store or a [shared store](../glossary.md#shared-store) keeps a local latest for each branch.
- `lore_revision_tree_commit` checks the loaded revision against the local latest (`lore-revision/src/commit.rs:4458-4466`). When the local latest is set and differs, the commit fails with a tip collision: `INTERNAL`, with the local latest in `new_tip_hash` (`lore/src/revision_tree/commit.rs:149-153`).
- A pipeline that loads the remote latest while its local latest is behind hits that failure, and nothing in the low-level API brings the local latest up to the remote.

**A pipeline on a shared store can't see where its repository instances stand on a branch.**

- Each repository instance over a shared store keeps its own current revision and current branch in the store, under keys derived from its instance id (`lore-revision/src/instance.rs:46-49, 119-131`). The revision anchors moved into the mutable store under [ADR-00014](../developing/decisions/00014-anchor-tracking-in-mutable-store.md), and the current-branch key joined them later (`lore-revision/src/instance.rs:48`).
- An instance's current revision moves only with that instance's own commands, while a commit from any instance moves the branch's local latest (`lore-revision/src/branch.rs:871-903`). So an instance stands behind the local latest once another instance commits to its branch, or after it syncs to an older revision.
- A pipeline that moves the local latest needs those positions first, so a forced move doesn't discard revisions an instance still stands on. One that works with a materialized instance needs that instance's revision to build on.
- Outside the file-system verbs, such as `lore repository instance list` (`lore-client/src/cli/commands/repository.rs:1720-1727`), reading those keys means re-deriving their internal key scheme, as for a branch's latest.

**Concurrent branch writers can leave a branch without its name, or under two.**

- Every writer of a branch's name-to-id mapping stores it without checking what it holds (`store_name_to_id` and `delete_name_to_id` in `lore-revision/src/branch.rs` at main 2270): a create, a restore of an archived branch, an archive, a reset, switch or clone, and Lore Server's push, list repair and repository delete.
- Two creates of one name can both succeed, leaving one branch without its name, and an archive can remove a mapping a concurrent writer pointed at another branch.
- A restore stores the branch's metadata and then its name's mapping (`create_at` in `lore-revision/src/branch.rs` at main 2270), and no compare-and-swap spans two keys ([system design §9.4](../explanation/system-design.md#94-atomicity-via-compare-and-swap)). Restores of one archived branch under two names can leave it mapped from both, and an archive racing a restore can leave the metadata and the names disagreeing.
- The create and archive verbs this proposal adds run on those writers, so they would inherit every race, in a caller with no working tree to show the damage.

Together, these gaps leave a caller that wants nothing on disk making a clone for every branch-level step, and the branch writers it would use racing each other.

## Goals / Non-Goals

### Goals

1. **Resolve revisions and branches without an instance, by structured identity.** Turn a revision hash, or a branch id plus a revision number or "latest," into the revision it names. Resolve a branch name to its id and record, including where the repository instances over a shared store stand on the branch.
2. **List a repository's branches without an instance,** both in the storage handle's local store and on the remote, with where the local store's repository instances stand on each.
3. **Create and archive branches without an instance.**
4. **Publish without an instance.** Push a branch's local history to the remote, and sync a branch's local latest to the remote latest or to a given revision.
5. **Make location explicit and uniform.** The storage API's local and remote flags decide which locations every verb consults. With no flag, a verb consults both, remote first. A verb never consults a location the flags exclude, and reports each location's answer in its own record instead of merging them. The exceptions are sync and push, which span both locations by nature: sync looks its target up at one location and always writes the local latest, and push always reads the local history and always writes to the remote.
6. **Set up a surface that can grow.** Define a namespace, conventions, and an amendment process, so later repository-level verbs join without each one re-arguing the shape.
7. **Keep the C API shape of the existing low-level surfaces:** sync and async variants of each verb, results through `lore_event_callback_config_t`, Rust entry points in the `lore` crate wrapped by `lore-capi`, and the full FFI error detail on the terminal event.
8. **Keep the remote's guarantees.** Push and archive through this surface pass the same authorization, branch protection, latest compare-and-swap, and revision verification on the remote as the file-system push and archive. Create passes the same checks as the remote branch creation that layer and link creation already perform.
9. **Keep branch names consistent under concurrent writers.** At most one live branch holds a name, and a branch is mapped only from the name its metadata holds or the name a restore in progress records. A writer that loses a race fails, or skips the name, and leaves the winner's mapping. A restore or archive, including one that stops part way, leaves the branch's metadata and its names agreeing, as the next restore or archive of the branch finds them, outside the windows Risks names.

### Non-Goals

- **Replacing the file-system branch verbs:** Interactive workflows keep `lore_branch_*`.
- **New RPCs, Lore Server protocol messages, or on-disk structures:** Every verb uses RPCs, message fields, and mutable keys that exist today. The Lore service IPC gains a command for each verb and an event for each event data type, as every C API addition does (see Compatibility).
- **A stateful repository handle:** See Alternatives Considered.
- **String [revision identifiers](../glossary.md#revision-identifier):** This surface takes structured identities only, never strings such as `main@latest`, `main@25`, or `~N`. Converting a string is the caller's job.
- **A merging sync:** `branch_sync` doesn't merge a divergent local latest with its target the way `lore sync` does when it produces a [merge revision](../glossary.md#merge-revision). A merging sync is a candidate for a later verb.
- **Cascading to [layers](../glossary.md#layer) and [links](../glossary.md#link):** The file-system push always pushes linked repositories' revisions, and the file-system create and archive can act on layers and links. Each verb here acts on the one repository it names. `branch_create` creates no layer or link branches, `branch_archive` archives none, and `branch_push` publishes no linked repository's revision.
- **Changing the revision tree API:** `lore_revision_tree_commit` stays local-only, and publishing is a separate call on this surface.
- **Separating event emission from the cores:** The cores keep sending the file-system verbs' events, and the verbs drop all but log lines, login prompts, and the push's progress and statistics (see "Changes beneath the verbs"). A later refactor makes the cores pure: each returns its outcome and sends no events, and the API layer sends the events. The file-system verbs then send theirs, and this surface becomes one more API over the same cores that sends only the events this proposal names. That refactor changes every file-system verb, so it's a change of its own.
- **History walks, diffs, revision listing, branch metadata or protection edits, and repository metadata such as the default branch, in the first version:** These are candidates for later verbs under "Extending the surface."
- **CLI commands:** The `lore` [CLI](../glossary.md#cli) gains nothing.

## Proposed Design

### Placement and namespace (Goal 6, Goal 7)

Every entry point lives in the `lore_revision_graph_*` namespace, named for the revision graph it operates on. The revision tree proposal already uses the name for the repository-level RPCs: "the existing revision-graph and storage RPCs" ([Non-Goals](2026-05-14-low-level-revision-api.md#non-goals)).

| Surface | Acts on | Identity | Namespace |
| --- | --- | --- | --- |
| **Storage system API** | Buffers | `(partition, address)` | `lore_storage_*` |
| **Low-level revision tree API** | One revision's tree | `lore_revision_tree_t` handle | `lore_revision_tree_*` |
| **Low-level revision graph API** | A repository's branches and revisions | `(store, repository)` | `lore_revision_graph_*` |
| **File-system-based revision control API** | A repository instance and its working tree | `globals.repository_path` | `lore_branch_*`, `lore_revision_*`, and others |

Verb names follow the noun-based pattern of `lore-capi`, `lore_revision_graph_<noun>_<verb>`, each with sync and `_async` variants. The Rust entry points live in a new `lore::revision_graph` module, beside `lore::revision_tree`.

### Stateless verbs (Goal 6, Goal 7)

There's no handle. Every verb's arguments carry the storage handle (`lore_store_t`) and the repository id (`lore_repository_id_t`, the revision tree proposal's alias for `lore_partition_t`). Each call builds a path-less `RepositoryContext` from those two values for each location it consults (see "Locations"), the way `lore_revision_tree_load` builds its context (`lore/src/revision_tree/handle.rs:325-347`), and drops them when the call returns. Each context has no paths and a zero instance id, and uses the storage handle's local stores. A context that writes carries an in-memory write token. A read verb's contexts carry none, so a write a core makes as a side effect fails instead of reaching the store.

Nothing has to persist between calls. Lore caches the remote connection for each [partition](../glossary.md#partition) (`lore-revision/src/protocol.rs:155-185`), and the storage handle keeps its remote endpoint and the storage session pools it opens (`lore/src/storage/remote.rs:22-77`). A pool serves one `globals.correlation_id` (`lore-transport/src/session.rs:545-547`), so a caller that passes the same correlation id on every call reuses one storage session per connection, as with the storage verbs. The verbs keep their sessions in those pools rather than on the shared connection, so the sessions end with the storage handle.

Every verb enters through the storage handle's in-flight guard, as the storage verbs do (`lore/src/storage/call.rs:68-88`), not through the plain handle lookup the tree load uses (`lore/src/revision_tree/load.rs:129-134`). So `lore_storage_close` behaves as it does for the storage API: it waits for the calls in flight, then flushes the stores (`lore/src/storage/close.rs:7-10`). A verb has no close of its own, so that drain is what flushes its local writes on a disk-backed store. A long push delays a close until the push finishes.

### Conventions (Goal 5, Goal 6, Goal 7)

Every verb on this surface follows these rules, and so does every verb added later.

**Arguments.** Each verb's arguments start with these fields:

- `id: u64`, a caller-chosen id echoed back in every event this surface defines for the call
- `store: lore_store_t`
- `repository: lore_repository_id_t`

The storage API documents its per-item `id` the same way (`lore/src/storage/get.rs:67-68`), and the revision tree verbs take the same per-call `id` (`lore/src/revision_tree/commit.rs:56-57`). A verb that takes a batch gives each item its own `id`, as the storage API does. The `id` is separate from `globals.correlation_id`, the string every API uses to key its remote sessions (`lore-transport/src/session.rs:500-546`). `COMPLETE` carries no `id`, as for every C API call, and the callback config that receives it identifies the call.

**Globals.** Every verb ignores `globals.force` and `globals.dry_run`. `branch_sync` and `branch_push` take `force` as an option instead, and no verb has a dry run.

**Locations.** The storage API's per-call flags (`globals.local`, `globals.remote`, `globals.offline`) and the flags the caller opened the storage handle with decide which locations a call consults. The surface shares the storage verbs' veto and rejection rules (`effective_flags`, `lore/src/storage/store.rs:217-246`). The storage API leaves the no-flag default to each verb, and its verbs differ. This surface picks one default for every verb that reports per location. `branch_create` and `branch_archive` follow `obliterate`, which acts on both locations and reports each (`lore/src/storage/obliterate.rs:5-9`). The read verbs consult both as well, whereas storage reads answer from the local store and fall back to the remote (`lore/src/storage/store.rs:114-126`). A branch's local latest and its remote latest are different facts, and a caller that syncs or pushes needs both. A caller that wants one location's answer sets its flag:

- **Neither flag:** The call consults the remote, if the storage handle has one, and the local store. The remote comes first: a write verb writes there before it writes to the local store, and a read verb emits the remote's records first.
- **`local` or `offline`:** The call consults the local store only.
- **`remote`:** The call consults the remote only. On a storage handle without a remote, the call fails with `NoRemote`. The mutable storage verbs reject that case with `InvalidArguments` (`lore/src/storage/mutable_load.rs:92-96`), and storage reads and the tree commit fall back to the local store without saying so (`lore/src/storage/store.rs:119-121`, `lore-capi/lore.h:13131-13136`). This surface names the missing remote instead, because a caller that asks for the remote's answer needs to learn there's no remote, not get the local store's answer in its place.
- **`local` and `remote` together, or flags that contradict the storage handle's:** The call fails with `InvalidArguments`.
- **`branch_sync` and `branch_push`:** These two verbs work across both locations by design, and their sections say what the flags mean for them.

The verbs that report per location (`revision_resolve`, `branch_resolve`, `branch_list`), and `branch_create` and `branch_archive`, reach each location on its own, and the work at one location never touches the other. A verb reaches the local store through a context with no remote, so it never contacts the remote there. On a store that fetches content on demand ([lazy fetch](../glossary.md#lazy-fetch)), a block the local store doesn't hold, whether never fetched or since evicted, counts as absent for the local answer. A corrupt block or a failed read still fails the call at that location. A read verb writes nothing to the local store, and work at the remote neither writes to the local store nor reads from it. The file-system verbs differ here in two ways:

- They cache a remote branch's name-to-id mapping and metadata, and the repository metadata, in the local store (`lore-revision/src/branch.rs:704-713`, `lore-revision/src/repository.rs:3841`). Their name lookup also writes a derived name-to-id mapping when it finds none and the derived id names a branch with a latest (`lore-revision/src/branch.rs:715-721`).
- Their remote reads cache what they fetch in the local immutable store, and cache blocks flagged for local caching even with caching off (`lore-storage/src/read.rs:353-366`). Their number lookup also writes the revision states it fetches into the local store (`lore-revision/src/revision.rs:1679`, `lore-revision/src/find.rs:317-340`).

Either would let the local answer find a branch or revision only because the remote answer fetched it.

**Records per location.** A verb that reports objects emits one record event per object per location consulted, remote first, tagged with `location: LoreBranchLocation` (`Local` or `Remote`, `lore-revision/src/interface.rs:1431-1436`). With no flag the verb reports both locations, and with a flag only that one, so the surface never merges two answers or picks one over the other.

Each location the call consults ends with a `LORE_EVENT_REVISION_GRAPH_LOCATION_COMPLETE` event after its records, which carries the number of records the call emitted for that location and the location's own `LoreErrorDetail`. Each storage item ends with its own complete event the same way ([ADR-00020](../developing/decisions/00020-storage-per-item-error-detail.md)). A caller tells three cases apart:

- **The location doesn't hold the object:** Its complete event reports no error and no records.
- **The call failed at the location:** Its complete event carries the error.
- **The call didn't consult the location:** No location complete event names it.

A failure at a location for any reason other than the object's absence fails the call:

- **Read verbs:** The verb still consults the other location, and `COMPLETE` carries the failed location's error, the remote's if both fail. Each location's complete event keeps its own error.
- **Write verbs (`branch_create`, `branch_archive`):** A failure at the remote stops the call before it writes to the local store, so no complete event for the local store follows.

A call that fails before it starts work at any location, such as on invalid arguments or a refused pre-check, emits no location complete event. A pre-check whose read fails, such as on a connection error, is a failure at that location, and its complete event carries the error.

**Local current.** A branch's local current is where the local store's repository instances stand on it. The mutable store keeps no current per branch. It keeps a current revision and a current branch per instance, keyed by instance id (`lore-revision/src/instance.rs:46-49, 119-131`). The instances on a branch are the registered instances (`lore-revision/src/instance.rs:477-496`) that have it as their current branch, read the way the branch-switch warning reads it (`lore-revision/src/instance.rs:519-531`). The verbs leave out a registration that names no usable instance id (see "Instances on a branch" under "Changes beneath the verbs"). The local current has three cases:

- **Instances on the branch:** The local current is each one's current revision. Several instances can be on one branch at different revisions, so a verb reports one instance record per instance.
- **No instance on the branch:** The local current is the local latest, which the branch's local record carries, and the verb emits no instance records.
- **No local branch:** There's no local current, and a caller uses the remote.

`branch_resolve` and `branch_list` emit a branch's instance records after its local record, a local `branch_sync` with no target emits them before its synced event, and a refused `branch_archive` emits the live ones.

**Revision selector.** A `lore_revision_selector_t` names a revision:

```rust
#[repr(C)]
pub struct LoreRevisionSelector {
    /// Non-zero: the revision with this hash. `branch` and `revision_number` must then be zero.
    pub hash: Hash,
    /// With a zero `hash`: the branch whose revision is selected.
    pub branch: BranchId,
    /// With a zero `hash`: the revision number on `branch`; 0 selects the branch's latest.
    pub revision_number: u64,
}
```

Exactly one of `hash` and `branch` is non-zero, and `revision_number` is zero with a hash. Anything else is `InvalidArguments`. The one exception is `branch_sync`, where an all-zero selector means the same as `{ .branch = branch }`: the latest of the branch the call names. Revision number 0 can mean "latest" without ambiguity, because the existing resolver treats it as naming no revision (`lore-revision/src/revision.rs:1641-1644`). Branches are always named by id (`lore_branch_id_t`). Branch names appear as input only where a name is the point of the call: `branch_resolve` and `branch_create`.

**Errors.** `LORE_EVENT_COMPLETE` carries a `LoreErrorDetail` with the real FFI code, message, and trace, as [ADR-00017](../developing/decisions/00017-ffi-error-detail-on-complete-event.md) defines for every call. The codes come from the registry in `lore-base/src/error.rs`, and this proposal allocates none. A refusal because of repository state comes from the repository-state block, 40 to 55: "well-formed and permitted, but the repository is in a state that refuses it." That block is this surface's "precondition failed."

| Code | Name | Raised when |
| --- | --- | --- |
| 3 | `InvalidArguments` | A malformed selector, invalid location flags, an invalid branch name, a parent or branch point a new branch can't start from, or a `branch_sync` target outside `branch` |
| 16 | `NotAuthenticated` | The remote doesn't accept the caller's credentials |
| 17 | `NotAuthorized` | The caller's identity lacks a permission the call needs, including a push to a protected branch and the linked-revision check in `branch_push` |
| 28 | `Disconnected` | The remote's connection dropped or the remote is unavailable. The transport maps Lore Server's `UNAVAILABLE` and `UNKNOWN` statuses to it (`lore-transport/src/error.rs:30-49`) |
| 29 | `NotConnected` | The call needs the remote, and the verb couldn't connect to it |
| 30 | `Maintenance` | The remote is in maintenance mode and declines work |
| 31 | `SlowDown` | The remote is overloaded and asks the caller to retry later. The transport maps Lore Server's `RESOURCE_EXHAUSTED` status to it (`lore-transport/src/error.rs:30-49`) |
| 41 | `BranchAdvanced` | `branch_push`: the remote latest isn't the revision the push builds on, and no fast-forward merge applies. `branch_sync`: the local latest isn't `expected_local_latest`, or changed during the call |
| 42 | `Divergent` | `branch_sync` without `force` when the target isn't a first-parent descendant of the local latest, including a local latest the target lacks and one the target reaches only through the second parent of a merge, or a `branch_create` whose branch point resolves to different revisions at the locations it creates on |
| 47 | `DeleteProtected` | `branch_archive` of a protected branch |
| 48 | `DeleteCurrent` | A `branch_archive` that would archive the branch locally while a live repository instance over the store has it as its current branch |
| 49 | `DeleteDefault` | `branch_archive` of the repository's default branch, or of a branch with an empty stack |
| 57 | `BranchAlreadyExists` | `branch_create` with a name a live branch holds at a location the call creates on, including the loser of two concurrent creates, a first `branch_push` of a branch whose name a different remote branch holds, a `branch_push` that would restore an archived branch whose name another branch now holds, or a `branch_sync` that would bring in a branch whose name a different local branch holds |
| 87 | `BranchNotFound` | The branch is absent from every location consulted, `branch_sync` names a branch absent from the location it looks the target up in, or `branch_push` names a branch with no local latest |
| 88 | `RevisionNotFound` | The selected revision is absent from every location consulted, `branch_create`'s branch point doesn't resolve at a location it creates on, a revision `branch_push` would offer points at a linked repository's revision the remote lacks, or `branch_sync`'s ancestry walk stops at a revision the local store lacks with no remote to read it from |
| 111 | `NoRemote` | The call needs the remote, and the storage handle has none |
| -1 | `Internal` | A remote refusal the verb can't attribute to a row above. The detail carries the remote's message |

The verbs produce these codes for refusals that come back from the remote too. Lore Server reports refusals as gRPC statuses, and today's client folds several of them into `Internal` (`lore-transport/src/error.rs:30-49`). A status alone doesn't always identify the refusal:

- **Hooks:** A server hook can refuse any request with one of six statuses, among them `FAILED_PRECONDITION`, `INVALID_ARGUMENT`, and `ABORTED` (`lore-server/src/grpc/mod.rs:568-584`).
- **`branch_delete`:** Answers `FAILED_PRECONDITION` for a protected branch and for the default branch alike (`lore-server/src/grpc/revision/v1/branch_delete.rs:144-157`).
- **`branch_create`:** Answers `ALREADY_EXISTS` for a taken name, `INVALID_ARGUMENT` for an argument it or the create core rejects, `RESOURCE_EXHAUSTED` for a store slowdown, and `INTERNAL` for any other failure of the create (`lore-server/src/grpc/revision/v1/branch_create.rs:196-204`). "Create and archive cores" under "Changes beneath the verbs" says why these statuses change. The deprecated `urc.rpc` create answers the same, except `INVALID_ARGUMENT` for a taken name (`lore-server/src/grpc/handlers/branch_create.rs:135-141`), so through `urc.rpc`, `INVALID_ARGUMENT` doesn't tell a taken name from a rejected argument.

So each verb checks what it can read before calling the remote: branch protection, the default branch, a live name, a valid name. After a refusal, it confirms a row's condition with a read before reporting that row. `branch_push`, for one, reports `BranchAdvanced` only when `branch_query` shows the remote latest isn't the revision the push built on.

Two conditions get different codes here than from the revision tree API:

- **A moved local latest:** The tree commit reports it as `Internal`, with the observed tip in `new_tip_hash` (`lore/src/revision_tree/commit.rs:149-153`), while `branch_sync` reports `BranchAdvanced`.
- **A missing revision:** The tree load reports it as `AddressNotFound`, `PayloadNotFound`, or `NotFound` (`lore/src/revision_tree/load.rs:50-57, 75-90`), while `revision_resolve` reports `RevisionNotFound`.

The tree verbs read blocks and answer in storage terms. This surface answers about branches and revisions, and the registry already has codes for those. A pipeline that uses both surfaces handles each code where it arises. This proposal leaves the tree API's codes as they are.

**Events.** The verbs add six event tags to the end of `lore_event_id_t` (`lore-capi/lore.h:3459`), so existing discriminants keep their values: `LORE_EVENT_REVISION_GRAPH_BRANCH`, `LORE_EVENT_REVISION_GRAPH_INSTANCE`, `LORE_EVENT_REVISION_GRAPH_REVISION`, `LORE_EVENT_REVISION_GRAPH_LOCATION_COMPLETE`, `LORE_EVENT_REVISION_GRAPH_BRANCH_SYNCED`, and `LORE_EVENT_REVISION_GRAPH_BRANCH_PUSHED`. A verb emits these events and the standard terminal events through the `lore_event_callback_config_t` the caller passes, as the storage and revision tree verbs do, along with the events any call can send: `LORE_EVENT_LOG` for its log lines ([ADR-00012](../developing/decisions/00012-log-dispatch.md)), and `LORE_EVENT_AUTH_URL` and `LORE_EVENT_AUTH_PENDING` when connecting needs an interactive login. The only other events are the push's progress and statistics, which `branch_push` forwards (see "Branch push"). The file-system verbs' other events don't reach the caller.

**Event naming.** A record's tag is `LORE_EVENT_REVISION_GRAPH_<NOUN>`, and an outcome's is `LORE_EVENT_REVISION_GRAPH_<NOUN>_<OUTCOME>`, such as `BRANCH_SYNCED` and `LOCATION_COMPLETE`. Each tag's data type is `LoreRevisionGraph<Noun>[<Outcome>]EventData`, exported as `lore_revision_graph_<noun>[_<outcome>]_event_data_t`. New tags go at the end of `lore_event_id_t`.

### Record types

Every event data type on this surface is `#[repr(C)]`, derives `Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode`, and renames its fields to camel case, as ADR-00022 requires of a `LoreEvent` payload. Its Boolean `u8` fields carry `#[serde(with = "u8_as_bool")]`, as the branch list entry's do (`lore-revision/src/branch.rs:145-150`). `stale` is a reason code, not a Boolean, and doesn't. The sketches below leave the attributes out.

```rust
/// LORE_EVENT_REVISION_GRAPH_BRANCH: one branch, as one location holds it.
pub struct LoreRevisionGraphBranchEventData {
    pub id: u64,                            // the call's `id`, echoed back
    pub location: LoreBranchLocation,
    pub branch: BranchId,
    pub name: LoreString,
    pub category: LoreString,
    pub latest: Hash,                       // the branch's latest at this location
    pub stack: LoreArray<LoreBranchPoint>,  // branch points, first = where this branch starts
    pub creator: LoreString,
    pub created: u64,
    pub protected: u8,
    pub archived: u8,
    pub divergent: u8,                      // the local latest may hold revisions the remote lacks; 0 on a Remote record
}

/// LORE_EVENT_REVISION_GRAPH_INSTANCE: one repository instance on a branch, as the local store records it.
pub struct LoreRevisionGraphInstanceEventData {
    pub id: u64,                            // the call's `id`, echoed back
    pub location: LoreBranchLocation,       // always Local: instances live in the local store
    pub instance_id: InstanceId,            // the instance's id from its registration
    pub path: LoreString,                   // the instance's working directory, as registered
    pub branch: BranchId,                   // the instance's current branch
    pub revision: Hash,                     // the instance's current revision
    pub stale: u8,                          // 0 live, 1 path missing, 2 superseded, 3 no checkout
}

/// LORE_EVENT_REVISION_GRAPH_REVISION: one revision, as one location resolves it.
pub struct LoreRevisionGraphRevisionEventData {
    pub id: u64,                            // the call's `id`, echoed back
    pub location: LoreBranchLocation,
    pub revision_hash: Hash,
    pub revision_number: u64,
    pub branch: BranchId,                   // the revision's own branch
}

/// LORE_EVENT_REVISION_GRAPH_LOCATION_COMPLETE: the call's outcome at one location.
pub struct LoreRevisionGraphLocationCompleteEventData {
    pub id: u64,                            // the call's `id`, echoed back
    pub location: LoreBranchLocation,
    pub records: u64,                       // record events the call emitted for this location, instance records included
    pub error: LoreErrorDetail,             // code 0 on success at this location, including when it doesn't hold the object
}
```

The branch record follows `LoreBranchListEntryEventData` (`lore-revision/src/branch.rs:128-151`), with four changes:

- That struct's `id`, the branch id, becomes `branch` here, and `id` echoes the call's `id`.
- `is_current` is gone, because it has no meaning without an instance.
- `protected` is new, from the branch's metadata.
- `divergent` is new. It reports the client's existing latest status: whether the local latest may hold revisions not in the remote's history (`lore-revision/src/branch.rs:808-820, 864-872`).

The instance record follows `LoreRepositoryInstanceEventData` (`lore-revision/src/instance.rs:941-962`), with three changes:

- `id` is new, and echoes the call's `id`.
- `location` is new, and always `Local`, so the record follows the per-location rule.
- `branch_name` is gone. Every record on this surface names a branch by id, and the branch record carries the name.

The instance record reports every registered instance on the branch, stale ones included, except a registration that names no usable instance id (see "Instances on a branch" under "Changes beneath the verbs"). `stale` carries the existing staleness check's outcome as its event flag, with the values `LoreRepositoryInstanceEventData` documents (`lore-revision/src/instance.rs:764-791, 838-887, 956-960`):

- **0:** The instance is live.
- **1:** The registered path no longer exists.
- **2:** The path holds a different instance, which superseded this one.
- **3:** The path holds no readable checkout.

An instance served through a virtual file system counts as live while its external directory under Lore's global data directory names it as such, whatever its path shows (`lore-revision/src/instance.rs:835-847`). `lore repository instance list` prints the non-zero values as `(stale)`, `(superseded)`, and `(no checkout)` (`lore-client/src/cli/commands/repository.rs:1720-1727`). The verbs never retire a registration. The file-system lookup retires the registrations it finds contradicted (`lore-revision/src/instance.rs:503-508`), and reporting with a flag leaves that decision to the caller.

### Verbs

| Verb | Input beyond `id, store, repository` | Emits before `COMPLETE` | Goal |
| --- | --- | --- | --- |
| `lore_revision_graph_revision_resolve` | `selector` | A revision record per location that holds the revision, and each location's complete event | 1 |
| `lore_revision_graph_branch_resolve` | `name` | A branch record per location, the local current's instance records, and each location's complete event | 1 |
| `lore_revision_graph_branch_list` | `include_archived`, `creator` | A branch record per branch per location, each local one followed by its instance records, and each location's complete event | 2 |
| `lore_revision_graph_branch_create` | `name`, `category`, `from` (selector) | A branch record per location created, and each location's complete event | 3, 8, 9 |
| `lore_revision_graph_branch_archive` | `branch` | A branch record per location archived and each location's complete event, or only the live instance records when they refuse the call | 3, 8, 9 |
| `lore_revision_graph_branch_sync` | `branch`, `target` (selector), `options` (`LoreRevisionGraphBranchSyncOptions`) | `REVISION_GRAPH_BRANCH_SYNCED`, after the local current's instance records for a local sync with no target | 4 |
| `lore_revision_graph_branch_push` | `branch`, `options` (`LoreRevisionGraphBranchPushOptions`) | Push progress and statistics, then `REVISION_GRAPH_BRANCH_PUSHED` | 4, 8 |

#### Revision resolve (Goal 1)

The verb resolves `selector` at each location it consults, remote first, one location at a time:

- **Latest on a branch:** That location's latest.
- **A revision number on a branch:** On the remote, the revision the `revision_list` RPC reports for that number (`lore-revision/src/revision.rs:1656-1678`). Locally, the revision a walk of the branch's parent history finds (`lore-revision/src/find.rs:207-254`).
- **A hash:** The revision, if that location holds its revision record.

The verb reads each resolved revision's record to report its number and its own branch. When the revision's metadata names no branch, or the local store holds the revision without its metadata, as it does for a revision cached from a `revision_list` page (`lore-revision/src/revision.rs:1411-1419`), `branch` is zero, meaning unknown. The selector's branch would be wrong for a revision a number walk reaches past the branch point, which belongs to the parent. It emits a revision record for each location that holds the revision. The two locations can disagree: number 25 on a branch whose local latest holds revisions not yet pushed can be a different revision locally than on the remote. The call succeeds when at least one location holds the revision and the verb failed at no location it consulted (see "Records per location"). When the verb succeeds at every location it consults and none holds the revision, the call fails with `BranchNotFound` if a branch selector names a branch no consulted location holds, and with `RevisionNotFound` otherwise. A caller loads the result with `lore_revision_tree_load`.

#### Branch resolve (Goal 1)

The verb resolves `name` to the branch it currently names at each location it consults, and emits one branch record for each location that holds it. The local current's instance records follow the local record. Name lookups are case-insensitive, because the name key hashes the lowercased name (`lore-revision/src/branch.rs:498-502`). An archived branch's name no longer maps to it (`lore-revision/src/branch.rs:668-681`), so this verb doesn't return archived branches. A restore that stops after it claimed the name, or a settle of it that stops before its release, leaves the name mapped to the branch until the next restore or archive of the branch releases it, and the verb returns the branch meanwhile.

#### Branch list (Goal 2)

The verb emits one branch record for each branch at each location it consults:

- **`include_archived`:** Also lists archived branches. An archived branch's record carries the latest the branch kept when it was archived, where the file-system list reports zero (`lore-revision/src/branch.rs:2281`).
- **`creator`:** When non-empty, lists only branches whose `creator` matches it exactly and case-sensitively, as Lore Server's filter does (`lore-server/src/grpc/revision/v1/branch_list.rs:43-44`). When empty, lists every creator.
- **At the remote:** The verb calls the `branch_list` RPC with the request's existing `creator` and `include_deleted` fields (`lore-proto/proto/lore/revision/v1/revision.proto:101-109`), which Lore Server honors (`lore-server/src/grpc/revision/v1/branch_list.rs:194-270`). It also applies `creator` to the records it receives, so a remote that ignores the field yields the same result. A remote that ignores `include_deleted` returns live branches only, and the verb can't detect that.
- **In the local store:** The verb enumerates the branch-id keys of the storage handle's mutable store (`lore-revision/src/branch.rs:2105-2115`), whose list leaves out a key holding zero (see "Removal on a swap to zero"), adds archived branches from the branch-metadata keys (`lore-revision/src/branch.rs:2242-2268`), and applies `creator` to each branch's metadata. The local current's instance records follow each local branch record. It lists the registered instances once per call.

#### Branch create (Goal 3, Goal 8, Goal 9)

The verb creates a branch named `name` in `category`, starting at the revision that `from` selects.

- **Validation:** The verb checks the name before it touches either location (`lore-revision/src/branch.rs:1499-1509`), and the parent and branch point at each location before it writes at either location. An invalid name, a parent that can't have children, or a zero branch point on a parent that isn't the default branch fails with `InvalidArguments`. The verb runs these checks itself rather than calling the create core, and "Create and archive cores" under "Changes beneath the verbs" says why.
- **Category:** An empty `category` means the default category, the one the file-system create uses (`lore-revision/src/branch.rs:1383`).
- **Parent and branch point:** A hash selector makes the parent that revision's own branch. A branch selector makes the parent that branch. The new branch's stack is the branch point followed by the parent's stack, the shape the file-system create builds from the current branch (`lore-revision/src/branch/create.rs:118-129`).
- **The parent's record:** The verb reads the parent's metadata at each location for the stack and category. When the local store lacks it, as on an in-memory store that resolved and pushed the parent through the remote, the verb uses the stack and category it read from the remote. With `local` alone, the verb doesn't consult the remote, so the local store must hold the parent's metadata, and otherwise the call fails with `BranchNotFound`.
- **Names:** `name` must be unclaimed: its name-to-id mapping is absent, as an archive leaves it, or a stale mapping whose branch has no metadata, as the file-system create treats it today (`lore-revision/src/branch.rs:677-678, 1548-1556`). A name a live branch holds fails with `BranchAlreadyExists`. An unclaimed name, including one an archived branch used to hold, gets a new branch with a new id, and the name maps to it. The archived branch stays reachable under its own id, through `revision_resolve` or through `branch_list` with `include_archived`.
- **Claiming the name:** Each location claims the name with a compare-and-swap on the name-to-id key, against the value it read. Every writer of that key goes through the same compare-and-swap:

  - The create (`lore-revision/src/branch.rs:1744`) and its restore path (`lore-revision/src/branch.rs:1607`)
  - The name lookup, where it caches a remote answer or falls back to a derived id (`lore-revision/src/branch.rs:710, 720`)
  - `branch_sync` when it brings a branch in
  - Clone, when it records a branch the local store lacks (`lore-revision/src/repository/clone.rs:1381`), and Lore Server's repair of a missing default-branch mapping in its branch list (`lore-server/src/grpc/handlers/branch_list.rs:124`), each skipping a name another writer claimed first
  - The archive core's removal of the mapping, which leaves alone a mapping that no longer names the archived branch (`lore-revision/src/branch.rs:1985-1992`)
  - Lore Server's repository deletion, which removes each branch's mapping against the id it read and ignores a mismatch, since the repository is going away (`lore-server/src/grpc/handlers/repository_delete.rs:150`, `lore-server/src/grpc/repository/v1/repository_delete.rs:115`)

  A create writes the branch's metadata before it claims the name, so no reader finds a create's mapping before its metadata. A mapping can still name a branch without metadata, as one does when a create's claim lands but reports a failure and the create removes its metadata. Such a mapping is stale, and the next create of the name claims over it. A create writes the metadata key with a compare-and-swap from zero, so of two creates of one branch id, one writes it and the other fails with `BranchAlreadyExists`. A create that loses the claim swaps its own metadata key back to zero, which removes the key, so the loser leaves no archived-looking branch behind. Of two concurrent creates of one name, one succeeds and the other fails with `BranchAlreadyExists`. A claim over a mapping that already names the claiming branch succeeds, so two writers of one branch, such as two syncs bringing in the same remote branch, both succeed and neither removes the metadata key. A file-system create or Lore Server create given the id of an archived branch restores that branch under the name it's given, which renames the branch where that isn't its own name. `branch_create` generates its own id, so it never restores a branch. Lore Server runs the same code (`lore-server/src/grpc/revision/v1/branch_create.rs:166-186`), so the claim closes the race for the file-system verbs and Lore Server too.

  Restores and archives of one branch also contend on a compare-and-swap of its metadata, which records whether the branch is archived:

  - A restore records the name it claims in a `restoring` key of the metadata, then claims the name, then finishes by removing that key and recording its time in a `restored` key. Of restores of one branch that race, at most one succeeds, and the others fail with `BranchAlreadyExists`: a restore that loses the record's swap fails before it claims anything, and one another writer settled fails as it finishes and releases its claim. A restore of a branch that has no latest revision and whose metadata records neither an archive nor a restore, which a restore doesn't rename, makes the claim alone.
  - A restore or an archive that finds a `restoring` key settles it, as does `lore branch create --force` of the name the key names: it swaps the key for a `released` key that names the same name, recording the time in `archived`, so that restore can't finish, then releases the name. The `released` key outlives the settle: every archive of the branch releases the name again, and the next restore releases it, then removes the key with the swap that records the restore. So while the key stays, a restore or a settle that stops part way, as when a process ends, holds no name the next restore or archive of the branch can't free; Risks names the case the key doesn't cover. Lore Server runs a create and a delete to their end even when a client drops the request.
  - An archive swaps the metadata, recording its time in an `archived` key and removing `restored`, then releases the name, so a restore that read the metadata before can't finish once the name is released.
  - Each release reads the metadata again first, and leaves a name a later restore holds; a restore's, settle's, archive's or protect's swap that finds the metadata changed by another writer reads it again. Risks names the windows those reads leave.
  - A rename refuses a branch whose old name still maps to it, and one whose metadata records neither an archive nor a restore and that has no latest revision, as an unfinished create leaves it: a create stores the latest revision only after its claim.
  - Metadata that says the branch is archived while its name maps to it, as an older version's restore or an archive stopped between its swap and its release leaves it, reads as live. Metadata with a `restored` key while its name doesn't map to the branch, as an older version's archive leaves it, reads as archived.
- **Locations:** The location flags decide where the branch exists, and neither flag means both. Before it creates anything, the verb resolves the branch point at every location the call creates the branch on. It must resolve to the same revision at each, and otherwise the call fails with `Divergent`, or with `RevisionNotFound` when a location can't resolve it. The verb creates the branch on the remote first, through the `branch_create` RPC (`lore-transport/src/traits.rs:256-264`), with the branch id it generates. It then creates the branch in the local store with the same id, name, category, creator, and stack. When the remote refuses the create but a confirming read shows a branch with the call's own id, the remote committed it and only its answer was lost, so the verb carries on as on success. Each location records its own `created` time (`lore-server/src/grpc/revision/v1/branch_create.rs:134`).
- **Creator:** The verb takes `creator` from the call's user id, as the file-system create does (`lore-revision/src/branch/create.rs:116`). Only the repository instance path sets that id (`lore-revision/src/repository.rs:2400-2405`), so when a call has none, the verb takes `globals.identity`, then the storage handle's identity, then the identity the remote connection authenticated as. Lore Server records the creator the client sends and reads the caller's identity only when none is sent (`lore-server/src/grpc/revision/v1/branch_create.rs:67-68, 137`). A create on both locations records the creator the remote returns in the local store too.
- **Latest status:** A branch created on both locations starts with its local latest in sync with the remote. A branch created locally only starts divergent, as the file-system create leaves it (`lore-revision/src/branch.rs:1738-1744`), until its first push.
- **A failed local create:** When the create succeeds on the remote and fails in the local store, `branch_sync` brings the branch into the local store under its id.
- **Committing to the new branch:** Load a tree at its latest, set the `branch` metadata key to the new branch's id, and commit. The id is the create record's `branch`, stored as a `LORE_METADATA_TYPE_CONTEXT` value, which is what the key holds (`lore-revision/src/metadata.rs:123-124`). A name there isn't read as a branch, and the commit lands on the parent instead (`lore-revision/src/commit.rs:4305-4308, 4320-4330`). The revision tree API already accepts a commit onto a branch whose branch point is exactly the loaded revision ([Implementation Notes](2026-05-14-low-level-revision-api.md#implementation-notes)).

The verb emits one branch record per location created.

#### Branch archive (Goal 3, Goal 8, Goal 9)

The verb archives `branch` at each location it consults:

- **Checks:** The verb checks protection and the default branch at each location, and refuses a protected branch with `DeleteProtected` and the default branch with `DeleteDefault`. At the remote, it reads the branch's metadata and the repository's default branch through `branch_query` and the repository `query` RPC (`lore-revision/src/repository.rs:3839-3860`) before it calls `branch_delete`. In the local store, it applies the archive core's checks (`lore-revision/src/branch.rs:1966-1984`), including the refusal of a branch with an empty stack, which it also checks at the remote. The reads before `branch_delete` are pre-checks: a refusal they find emits no location complete event (see "Records per location"), while a refusal of the `branch_delete` RPC itself is a failure at the remote, and the remote's complete event carries it.
- **At the remote:** The verb archives there first, through the `branch_delete` RPC (`lore-transport/src/traits.rs:266`).
- **In the local store:** The verb archives as the archive core does: its record write swaps the metadata to record `archived` and remove `restored`, then removes the name-to-id mapping, and keeps the latest revision (`lore-revision/src/branch.rs:1934-2004`). As in the archive core, the verb first settles a `restoring` key and releases a `released` name, and then archives a branch whose name still maps to it.
- **Already archived:** A location where the branch is already archived, once any `restoring` key is settled and any `released` name released, counts as archived there. The verb emits its record with `archived` set and writes nothing more, as Lore Server's `branch_delete` answers success for an archived branch, so a retry after a partial failure succeeds.
- **Instances on the branch:** When the call will archive the branch locally, the verb reads which repository instances over the store have the branch as their current branch, before it archives anything. A live one, whose instance record has `stale` 0, refuses the call: the verb emits the live instances' records, so the caller sees which instances stand on the branch, and fails with `DeleteCurrent` before either location changes. The file-system archive refuses only its own instance's current branch (`lore/src/branch.rs:1195-1209`). This verb extends the rule to every instance over the store, because it acts for none of them.

The verb emits one record, with `archived` set, for each location archived. When the archive succeeds on the remote and fails in the local store, including on one of the archive core's own checks, the local store's complete event carries the error, and the caller retries with the `local` flag.

#### Branch sync (Goal 4)

The verb syncs the local latest of `branch` to a target revision. It's the part of `lore sync [revision]` that applies without a working tree, without the merge.

- **Target:** `target` selects the revision, and an all-zero selector selects the branch's latest, as `{ .branch = branch }` does. The flags decide where the verb looks the target up. With no flag or `remote`, the verb looks it up on the remote. With `local` or `offline`, or with no flag on a storage handle without a remote, it looks it up in the local store, for example a revision another repository instance committed to a shared store. `remote` on a storage handle without a remote fails with `NoRemote`. Whatever the flags, the verb writes only the branch's local latest and what goes with it: the latest status and latest history that the local latest's compare-and-swap writes (`lore-revision/src/branch.rs:887-911, 938-981`), and the last-sync revision when it records the branch as in sync with the remote. Two exceptions, below: bringing a branch in writes its metadata and name, and a walk at the remote keeps the history it fetches in the local store.
- **What a target may name:** A hash target must be a revision whose own branch is `branch`, or the branch point `branch` starts from. A branch-form target must name `branch` itself. Any other target fails with `InvalidArguments`, and a target the location doesn't hold fails with `RevisionNotFound`. A branch that location doesn't hold fails the call with `BranchNotFound`.
- **A local sync with no target:** An all-zero target that the verb looks up in the local store changes nothing. The verb emits the local current's instance records, then the synced event with the local latest as both `previous_local_latest` and `local_latest`. A caller on a shared store sees each instance's current revision next to the local latest, and builds on the local latest or names a target. These are the instance records `branch_resolve` with `local` returns, keyed by branch id, so a caller that syncs reads what a local sync would build on without a second verb. When the local store doesn't hold the branch, the call fails with `BranchNotFound`, and the caller uses the remote.
- **Fast-forward:** The verb moves the local latest to the target when the local latest is zero or the target descends from it along first parents, which the existing branch-point walk checks (`lore-revision/src/history.rs:89-261`). A target that reaches the local latest only through a merge's second parent reports `Divergent`, and `force` moves past it. When the verb looks the target up on the remote, the walk reads history from the remote and keeps what it fetches in the local store, as today (`lore-revision/src/history.rs:167-225`), which suits a verb that writes the local store anyway. When it looks the target up locally, the walk reads only the local store. When neither side of the walk can advance, one of them stuck on a revision the local store lacks, and the walk hasn't found the common ancestor, the verb fails with `RevisionNotFound`, naming that revision, or with the remote's error when the remote failed the request for that side, and `force` skips the walk. The remote is asked for a side once until a page moves it. A side that stops early doesn't fail the walk while the other side can still reach the common ancestor.
- **Moving back or across:** When the local latest holds revisions the target lacks, the verb fails with `Divergent`, where `lore sync` would merge. With `force`, it moves the local latest anyway, and the revisions it leaves behind stay in the immutable store, unreferenced by the branch.
- **`expected_local_latest`:** When non-zero, the verb proceeds only if the local latest equals it, and fails with `BranchAdvanced` otherwise. Zero means no expectation, so the option can't require an absent local latest. On a shared store, a forced sync with this set leaves behind only revisions the caller knows about, never revisions another instance hasn't pushed yet.
- **In sync with the remote:** When the verb consults the remote and the target is the remote latest, it records the branch as in sync with the remote: it clears the latest status and stores the last-sync revision (`lore-revision/src/branch.rs:891-901, 905-907`).
- **Bringing the branch in:** When the local store lacks the metadata of a branch the remote has, or its name-to-id mapping doesn't name the branch, including the partial record a `branch_create` that failed in the local store leaves, the verb copies what's missing from the remote's record into the local store, under the same id: the metadata blob in the immutable store, the metadata key, and the name-to-id key. It claims the name with the same compare-and-swap as `branch_create`, and fails with `BranchAlreadyExists` when a different live local branch holds the name. That includes a branch archived in the local store but live on the remote: the sync settles a `restoring` key in the local metadata, or releases a `released` name, the branch's own name included, replaces the local metadata with the remote's, with a compare-and-swap from the value it read, and maps its name to it again. The file-system writes that cache the remote's metadata locally, the name lookup's and the branch metadata commands', settle or release such a key first too, and swap from the value they read. A branch the remote has archived too gets its metadata but no name. The verb then reads the local latest as usual. A store that committed and pushed through the remote can hold a local latest without the branch's metadata, because a tree commit writes the latest and its status and history, never the metadata (`lore-revision/src/commit.rs:4545-4554`).
- **Concurrency:** The verb moves the local latest with the existing compare-and-swap (`lore-revision/src/branch.rs:871-903`). A concurrent tree commit on the same store either reports a tip collision or makes the sync fail with `BranchAdvanced`.
- **The synced event:** `LORE_EVENT_REVISION_GRAPH_BRANCH_SYNCED` fires before `COMPLETE` once the verb has read the local latest, on success and on failure alike. On success, `local_latest` is the new local latest. When the call fails after that read, `previous_local_latest` and `local_latest` both hold the local latest the verb last observed, so a caller can retry with `expected_local_latest` set to it without another call. A call that fails before that read, such as on invalid arguments, emits only `COMPLETE`.

The options follow the shape of `LoreRevisionTreeCommitOptions` (`lore/src/revision_tree/commit.rs:41-49`), as the push's do. The options and the synced event:

```rust
#[repr(C)]
pub struct LoreRevisionGraphBranchSyncOptions {
    pub force: u8,                   // move the local latest even when the target isn't a fast-forward
    pub expected_local_latest: Hash, // non-zero: proceed only while the local latest equals it
}

/// LORE_EVENT_REVISION_GRAPH_BRANCH_SYNCED
pub struct LoreRevisionGraphBranchSyncedEventData {
    pub id: u64,
    pub branch: BranchId,
    pub previous_local_latest: Hash,
    pub local_latest: Hash,
}
```

A caller needs this verb in three situations:

- **Before committing on a lagging store:** A disk-backed or shared store keeps a local latest that can lag the remote latest.
- **After a rejected push:** On any store, including an in-memory store the caller reuses, the local latest keeps the revisions the remote refused. A forced sync with `expected_local_latest` set to that local latest clears them.
- **Building on another instance's work:** On a shared store, a `local` sync to a given revision moves the local latest to a revision that's in the store but not on the remote.

An in-memory store opened for one job starts with no local latest, so its first commit on a branch needs no sync.

#### Branch push (Goal 4, Goal 8)

The verb publishes the local history of `branch` to the remote. It always writes to the remote, so `local` and `offline` fail with `InvalidArguments`, `remote` means the same as no flag, and a storage handle without a remote fails with `NoRemote`. The branch needs a local latest. When the local store lacks the branch's metadata, the push reads it from the remote each time, as the file-system push does (`lore-revision/src/branch/push.rs:751-761`, `lore-revision/src/branch.rs:984-1000, 1242-1259`).

The verb reuses the history walk and upload of the file-system push (`lore-revision/src/branch/push.rs:922-1165`), without that push's layer and link steps (`lore-revision/src/branch/push.rs:531-532`):

- It walks the branch's local history back to the remote latest.
- It uploads each revision's [fragments](../glossary.md#fragment) against its parent, querying the remote for the fragments it already holds.
- It creates the branch on the remote on its first push. When a different remote branch holds the name, the push fails with `BranchAlreadyExists`.
- It offers each revision through the `branch_push` RPC (`lore-transport/src/traits.rs:278-284`). Lore Server checks branch protection (`lore-server/src/grpc/handlers/branch_push.rs:1031-1043`), verifies the revision, and moves the remote latest with a compare-and-swap (`lore-server/src/grpc/handlers/branch_push.rs:1286-1335`).

The verb has two options:

```rust
#[repr(C)]
pub struct LoreRevisionGraphBranchPushOptions {
    pub force: u8,              // overwrite a remote latest that has moved
    pub fast_forward_merge: u8, // let Lore Server merge onto a moved remote latest
}
```

- **`fast_forward_merge`:** When the remote latest has moved and a merge onto it has no conflicts, Lore Server merges instead of refusing (`lore-server/src/grpc/handlers/branch_push.rs:1352-1546`).
- **`force`:** Overwrites a remote latest that has moved. Lore Server still applies branch protection, which only identities with the push-protected permission pass (`lore-server/src/grpc/handlers/branch_push.rs:826-848, 1031-1043`). Like every verb here, the push ignores `globals.force` and `globals.dry_run`, which the file-system push reads (`lore-revision/src/branch/push.rs:1012, 1037, 1186-1187`).

The verb forwards two of the push core's events, as `lore_branch_push` emits them (`lore-revision/src/branch/push.rs:461-479, 1960-1979`). Their payloads are counts, and they don't carry the call's `id`:

- **`LORE_EVENT_BRANCH_PUSH_FRAGMENT_PROGRESS`:** Fires every `globals.event_interval_ms` while the push uploads fragments.
- **`LORE_EVENT_BRANCH_PUSH_STATS`:** Fires once when the push finishes, when `globals.stats` asks for statistics.

`LORE_EVENT_REVISION_GRAPH_BRANCH_PUSHED` fires before `COMPLETE` once the verb has read the remote latest, on success and on failure alike. A call that fails before that read, such as on invalid arguments or `NoRemote`, emits only `COMPLETE`.

```rust
/// LORE_EVENT_REVISION_GRAPH_BRANCH_PUSHED
pub struct LoreRevisionGraphBranchPushedEventData {
    pub id: u64,
    pub branch: BranchId,
    pub revision_hash: Hash,      // the last revision this call published; zero if it published none
    pub revision_number: u64,     // its revision number on the remote; 0 when `revision_hash` is zero
    pub fast_forward_merged: u8,  // Lore Server merged onto a moved latest; `revision_hash` is that merge revision
    pub remote_latest: Hash,      // the remote latest when the call finished
    pub local_latest: Hash,       // the local latest when the call finished
}
```

- **On success:** `revision_hash` is the published revision, `remote_latest` equals it, and the verb updates the local latest to it, after a fast-forward merge too. It updates the local latest only from the value it pushed: a tree commit that moves the local latest while the push runs stays in place, the push still succeeds, and `local_latest` reports the local latest the verb found.
- **Nothing to publish:** When the remote latest already is the local latest, the call succeeds without offering a revision. `revision_hash`, `remote_latest`, and `local_latest` all name that revision, so a caller continues from `revision_hash` either way.
- **The published hash can differ from the hash commit returned:** After a fast-forward merge, the remote latest is a merge revision Lore Server created. Lore Server can also re-serialize a revision with a corrected revision number (`lore-server/src/grpc/handlers/branch_push.rs:1275-1284`), and the push then re-parents its later revisions onto it (`lore-revision/src/branch/push.rs:1199-1226`). Either way, `revision_hash` is the revision to build on next.
- **When the remote latest moved:** If the remote latest isn't the revision the push builds on, and no fast-forward merge applies because the caller didn't ask for one or it would conflict, the push fails with `BranchAdvanced`. `remote_latest` names the revision to reload from. The local latest doesn't move, and `local_latest` reports it. A push that published some of its revisions before the remote moved reports the last of them in `revision_hash`. Today the client reports these refusals as `Internal` (`lore-revision/src/branch/push.rs:1037-1050, 1317-1326`, `lore-server/src/grpc/revision/v1/branch_push.rs:180-196`). The verb reads the remote latest with `branch_query`, reports `BranchAdvanced` when the read confirms the remote latest moved, and otherwise reports `Internal` with the remote's message.
- **An archived branch:** Lore Server refuses a push to a branch it has archived with `NOT_FOUND`. The verb, like the file-system push, restores it with `branch_create` under its id and offers the revision again (`lore-revision/src/branch/push.rs:1016-1033, 1061-1074`). When another branch now holds its name, the push fails with `BranchAlreadyExists`.

The push has two preconditions:

- **Content:** The revisions' tree blocks and leaf payloads must be in the storage handle's store or already on the remote. A tree commit and storage puts with `remote_write` set leave the push little to upload.
- **Linked repositories:** The push publishes this repository's revisions only. The file-system push also pushes the revisions of linked repositories that each revision points at (`lore-revision/src/branch/push.rs:1139, 1197`, function at `1786-1830`), but this verb requires those revisions to be on the remote already. It checks each one with the storage protocol's existence query, the query the push already sends for fragments (`lore-revision/src/branch/push.rs:1943-1952`), against the linked repository's partition under the caller's identity. A missing one fails the push with `RevisionNotFound`, naming the linked repository and revision, before the verb offers the revision that points at it. An identity without read access to the linked repository fails with `NotAuthorized`. The caller pushes the linked repository's branch first, with its own call.

### Changes beneath the verbs (Goal 8, Goal 9)

The verbs reuse Lore's branch model, RPCs, and cores, but the cores assume the file-system verbs' behavior in places. Each change below stays inside the Lore library, the transport client, the core Lore Server shares with them, that core's call sites in Lore Server, Lore Server's `BranchPush` and `BranchMetadataSet` handlers, and the local and DynamoDB mutable stores. None changes a Lore Server protocol message or field, or an on-disk structure.

- **Per-location contexts:** The verbs reach the local store through a context with no remote, which the context constructor already allows (`lore-revision/src/repository.rs:661-667, 674-683`). At the remote, they skip the cores' local caching of mutable keys (`lore-revision/src/branch.rs:704-713`, `lore-revision/src/repository.rs:3852`) and the number lookup's write of fetched revision states (`lore-revision/src/revision.rs:1679`, `lore-revision/src/find.rs:317-340`). Their remote context's immutable reads skip the local store (`lore-storage/src/options.rs:107-110`) and suppress local caching, even for blocks flagged for local caching. That takes a new `ReadOptions` field (`lore-storage/src/options.rs:100-131`, `lore-storage/src/read.rs:353-355, 1815-1817`) and a context setting that `read_options_from_repository` passes to the cores' reads (`lore-revision/src/immutable.rs:134-147`). At the remote, they call the RPCs themselves rather than the cores that read the `remote` flag from the globals, such as the resolver's branch lookup (`lore-revision/src/branch.rs:1032-1040`). In the local store, a read verb skips the name lookup's derived-id write (`lore-revision/src/branch.rs:715-721`).
- **Local misses:** Reads in the local store tell a block the store doesn't hold apart from a corrupt entry or a failed read, through another `ReadOptions` field that only the new verbs set, and only for the local store. All three come back as `AddressNotFound` today (`lore-storage/src/read.rs:244-257, 461-462, 518-522`). Every other read keeps today's answer, including Lore Server's, whose `AddressNotFound` drives the push's re-upload recovery (`lore-revision/src/branch/push.rs:1928-1936`).
- **Ancestry walk:** When it has no remote to ask, the shared branch-point walk fails once neither side can advance and one of them is stuck on a revision the local store lacks, reporting that revision, instead of looping (`lore-revision/src/history.rs:167-253`). The file-system push, sync, and revision lookup run the same walk (`lore-revision/src/branch/push.rs:719, 877`, `lore-revision/src/revision/sync.rs:603`, `lore-revision/src/revision.rs:1600`), so a push or sync without a remote now fails instead of looping, and a revision lookup falls back to the local latest, as it already does on any walk error (`lore-revision/src/revision.rs:1611-1616`). Compatibility lists the change.
- **Resolution per location:** The resolver merges both locations into one answer when it searches both (`lore-revision/src/revision.rs:1597-1622, 1656-1705`) and returns only a hash. `revision_resolve` doesn't call it, because the resolver's fallback walk reads and writes the local store (`lore-revision/src/revision.rs:1634-1710`). At the remote, the verb calls `branch_query` and `revision_list` itself. In the local store, it reads the local latest and walks the parent history with a walk that reads only the local store and writes nothing. It then reads the revision record for the rest.
- **Push core:** Gains an option to check linked revisions on the remote instead of pushing them (`lore-revision/src/branch/push.rs:1786-1830`), takes `force` and `dry_run` as parameters instead of reading the globals (`lore-revision/src/branch/push.rs:1012, 1037, 1186-1187`), and returns its outcome, which today reaches the caller only as events (`lore-revision/src/branch/push.rs:613-619, 1299-1310, 1351-1360`). Its history walk and upload become reachable from the `lore` crate (`lore-revision/src/branch/push.rs:785, 922`). For this surface it also updates the local latest only from the value it pushed, and after a fast-forward merge too (see "Branch push"). The file-system push keeps today's behavior.
- **Create and archive cores:** Each gets a record write the verbs call after their own checks, and the cores keep their signatures and call it. The create's record write takes the latest status to record, which today is always divergent (`lore-revision/src/branch.rs:1752`), and refuses a zero id with `InvalidArguments` (`lore-revision/src/branch.rs:2847-2851`), as the create core does before it writes anything. The archive's record write swaps the metadata to record the archive before it releases the name. The create core's checks report an invalid name, a zero parent, the branch itself as parent, a parent that can't have children, and a zero branch point on a parent that isn't the default branch as `InvalidArguments`, where today they report `Internal` (`lore-revision/src/branch.rs:1525-1527, 1623-1631, 1657-1671`). These codes let Lore Server's create tell an argument the core rejects, which it answers `INVALID_ARGUMENT`, from a store failure, which it answers `INTERNAL` (see "Create statuses" under Compatibility). It must tell them apart, because a create now fails on a failed read (see "Behavior changes" under Compatibility). `branch_create` skips the core, which records the latest as divergent and lets a parent whose metadata it can't read pass (see "Validation" under "Branch create"). The transport's `branch_create` returns the branch record Lore Server sends, which the client drops today (`lore-transport/src/grpc/revision_client.rs:87-90`).
- **Name claim:** Every writer of the name-to-id key claims it with a compare-and-swap against the value it read, on the client and in Lore Server. That covers the create and its restore path, the name lookup that caches a remote answer or falls back to a derived id, the sync that brings a branch in, clone, the archive core's removal, and Lore Server's default-branch repair and repository deletion (`lore-revision/src/branch.rs:650-666, 710, 720, 1607, 1744, 2001`, `lore-revision/src/repository/clone.rs:1381`, `lore-server/src/grpc/handlers/branch_list.rs:124`, `lore-server/src/grpc/handlers/repository_delete.rs:150`, `lore-server/src/grpc/repository/v1/repository_delete.rs:115`).
- **Restores:** A create given an archived branch's id restores it. `lore branch switch`, `lore branch reset` and Lore Server's push leave an archived branch archived, and the push answers `NOT_FOUND` for one. A sync, a name lookup, or a switch or reset by id brings in the remote's record of a branch archived in the local store but live on the remote, which maps its name again. The `archived`, `restored`, `restoring` and `released` metadata keys, which archives and restores write, are read-only. Restores and archives contend on the branch's metadata as Branch create describes, and Compatibility lists the breaking changes.
- **Removal on a swap to zero:** A mutable-store key holds zero when it's absent. In both stores, a store of zero and a swap to zero from the value the key holds remove the key, atomically with every other write of the key and durably through a flush and a reopen. A swap from zero to zero on an absent key leaves no entry or item, a swap from a value to itself leaves the key unchanged, and a list leaves out a key holding zero. The name claim makes the archive's and Lore Server's deletes of a mapping, and the create's rollback, swaps to zero, so each removes the key. DynamoDB makes a swap to zero a delete on the swap's condition, and a swap from a value to itself a strongly consistent read (see Metrics). An entry or item holding zero that an older version wrote reads as absent, and the next removal removes it.
- **Branch list transport:** `Revision::branch_list` takes the `creator` and `include_deleted` values (`lore-transport/src/traits.rs:287`, `lore-transport/src/grpc/revision_client.rs:116-151`). The client keeps the branch data its conversion drops today, the deleted flag and the metadata (`lore-transport/src/grpc/revision_client.rs:384-394`), which the remote record needs for `archived` and `protected`.
- **Branch query transport:** `Revision::branch_query` returns the full branch record Lore Server sends. Today the client keeps only the id, latest, metadata, and deleted flag (`lore-transport/src/grpc/revision_client.rs:165-224`), so the verbs would need another read for a record's name, category, creator, and stack. With the full record, a branch record at the remote costs at most one blob read.
- **Refusal mapping:** The transport maps three of Lore Server's gRPC statuses straight to a code: `UNAUTHENTICATED` to `NotAuthenticated`, `PERMISSION_DENIED` to `NotAuthorized`, and `RESOURCE_EXHAUSTED` to `SlowDown` (`lore-transport/src/error.rs:30-49`). The mapping applies to every client call, not only this surface's (see Compatibility). A server hook can't answer `UNAUTHENTICATED` (`lore-server/src/grpc/mod.rs:568-584`), and a hook's `PERMISSION_DENIED` is a permission refusal too. Every other row needs a pre-check or a confirming read, as "Errors" describes. A refusal that neither identifies surfaces as `Internal` with the remote's message.
- **Instances on a branch:** The verbs list the registered instances and read each one's current branch and current revision without retiring any registration. The file-system lookup retires the registrations it finds contradicted (`lore-revision/src/instance.rs:503-508`), and the archive's context carries a write token (`lore/src/revision_tree/handle.rs:344`), so reusing that lookup there would retire them before the archive decides anything. The verbs also leave out a registration whose blob names no usable instance id, which reads as the zero instance id (`lore-revision/src/instance.rs:427-435, 731-740`): the anchors under the zero instance id belong to no instance, so they don't hold that registration's position (`lore-revision/src/instance.rs:150-155, 872, 889-893`). Such a registration never makes `branch_archive` refuse. `lore repository instance list` and the branch-switch warning still report such a registration, on the branch the zero instance id's current-branch anchor names (`lore-revision/src/instance.rs:349-351, 365-366, 549-556`).
- **Events:** The cores send their file-system events through the call's dispatcher, such as `BranchCreate`, `BranchArchive`, `RevisionResolve`, and the `BranchPush*` events (`lore-revision/src/branch.rs:1757-1764, 2005-2010`, `lore-revision/src/revision.rs:1576-1585, 1850-1859`). The verbs keep them from the caller: the call's dispatcher drops the cores' events before queuing them for the callback (`lore-revision/src/relay.rs:197-209`), except log lines and login prompts, which every verb passes on, and the push's progress and statistics events, which `branch_push` forwards. The filter is interim. Once the cores send no events (see Non-Goals), the verbs drop nothing, and a caller sees the same events.

### Use with the revision tree API

The sketch below follows the conventions of the C API example in the revision tree proposal: sync variants, and a `capture(&dest)` helper that copies a payload out of the callback. A pipeline on an in-memory store resolves a branch, edits its remote latest, publishes the result, and recovers when the remote latest has moved.

```c
const lore_global_args_t   globals = { /* identity, logging, a stable correlation_id */ };
lore_store_t               store   = /* lore_storage_open: in-memory, with a remote */;
const lore_repository_id_t repo    = /* repository id */;
lore_revision_graph_branch_event_data_t        main_branch;
lore_revision_graph_branch_pushed_event_data_t pushed;
lore_revision_tree_t                           tree;
lore_hash_t                                    committed;

/* 1. Branch name to id and remote latest. */
lore_global_args_t remote_only = globals; remote_only.remote = 1;
lore_revision_graph_branch_resolve(&remote_only,
    &(lore_revision_graph_branch_resolve_args_t){ .store = store, .repository = repo,
                                                  .name = { "main", 4 } },
    capture(&main_branch));

/* 2. Load, edit, and commit through the revision tree API (see its proposal). */
lore_revision_tree_load(&globals,
    &(lore_revision_tree_load_args_t){ .store = store, .repository = repo,
                                       .revision_hash = main_branch.latest },
    capture(&tree));
/* ... lore_storage_put, lore_revision_tree_add / modify / delete ... */
if (lore_revision_tree_commit(&globals,
        &(lore_revision_tree_commit_args_t){ .handle = tree, .options = { .remote_write = 1 } },
        capture(&committed)) != 0) {
    /* handle the commit failure */
}
lore_revision_tree_close(&globals, &(lore_revision_tree_close_args_t){ .handle = tree },
                         capture(NULL));

/* 3. Publish; Lore Server merges onto a moved latest when the merge is conflict-free. */
int status = lore_revision_graph_branch_push(&globals,
    &(lore_revision_graph_branch_push_args_t){ .store = store, .repository = repo,
        .branch = main_branch.branch, .options = { .fast_forward_merge = 1 } },
    capture(&pushed));

if (status == 0) {
    /* Continue from pushed.revision_hash; after a fast-forward merge it's the merge revision. */
} else if (status == 41 /* BranchAdvanced, lore-base/src/error.rs */) {
    /* 4. The remote latest moved and the merge conflicted: move the local latest to the
       remote latest the push read, dropping the local revision, then rebuild on it and
       push again. */
    lore_revision_graph_branch_sync(&globals,
        &(lore_revision_graph_branch_sync_args_t){ .store = store, .repository = repo,
            .branch = main_branch.branch, .target = { .hash = pushed.remote_latest },
            .options = { .force = 1, .expected_local_latest = pushed.local_latest } },
        capture(NULL));
}
```

`revision_resolve` covers the cases a branch record doesn't, such as a revision number on a branch. To start a new branch from the same revision, the pipeline follows these steps:

1. Call `lore_revision_graph_branch_create` with `.from = { .branch = main_branch.branch }` and a name.
2. Load a tree at the new branch's latest.
3. Set the `branch` metadata key to the new branch's id, the create record's `branch`, as a `LORE_METADATA_TYPE_CONTEXT` value.
4. Commit, then push the new branch's id.

### Extending the surface (Goal 6)

This namespace is the home for repository-level verbs that need no repository instance. An amendment to this proposal can add a later verb when it meets all of these conditions:

- It's stateless and takes `(store, repository)`.
- It follows every rule under "Conventions": location flags, separate work at each location, records per location, the revision selector, branch ids, error detail, and event naming.
- It needs no new RPC, Lore Server protocol message, or on-disk structure. Its command and events join the Lore service IPC by appending, as this proposal's do.
- It doesn't change Lore Server's behavior.
- It doesn't change the trust model.

An amendment adds a subsection to "Extensions" below, giving the verb, its arguments, its events, and its error codes, and updates `updated` in the frontmatter. Its author proposes it as a [change request](../glossary.md#change-request) or pull request against this file. Reviewers discuss and accept it under the LEP process in `GOVERNANCE.md`, like the proposal itself, before anyone implements the verb. The subsection stays marked Draft until then. If reviewers reject the amendment, its subsection stays in the file, marked Rejected, as rejected proposals stay in the tree. A verb that breaks a condition above, such as one that needs a protocol or format change, changes Lore Server's behavior, or introduces a stateful handle, needs a new proposal, and so does any change to the conventions themselves.

These verbs are likely candidates, though this proposal proposes none of them:

- Reading a branch record by id
- Reading and editing branch metadata and protection
- Reading repository metadata, such as the default branch
- Restoring an archived branch
- Listing a branch's revisions
- Walking history and finding merge bases
- Diffing two revisions
- A merging sync of a divergent local latest

## Compatibility

- **Wire format** — No change between a client and Lore Server. The Lore service IPC gains commands and events.
  - **Lore Server protocol:** No message, field, framing, or serialization changes. The verbs use the RPCs behind the revision transport methods `branch_query` (the `BranchGet` RPC), `branch_list`, `branch_create`, `branch_delete`, `branch_push`, and `revision_list` (`lore-transport/src/traits.rs:256-292`), the repository service's `query` (`lore-transport/src/traits.rs:326-331`), and the existing storage protocol.
  - **Lore service IPC:** A process that relays its C API calls to the Lore service sends each call as a `MessageToServer` holding a `LoreCommand`, and gets the call's events back as `MessageToClient` values holding a `LoreEvent` (`lore/src/remote/message.rs:164-209`). Both travel as `bitcode` under message protocol version 2 ([ADR-00022](../developing/decisions/00022-service-ipc-bitcode.md)). The proposal appends seven `LoreCommand` variants, one for each verb's args type, after the last existing variant (`lore/src/remote/command.rs:9-174`), and six `LoreEvent` variants after the last existing event (`lore-revision/src/event.rs:839-1339`). The framing, the header, and the protocol version byte don't change (`lore/src/remote/message.rs:93-104`).
  - **Mixed versions:** `bitcode` identifies a variant by its position, so appending leaves every existing command and event where it was. An older service rejects a new command instead of running it as another one: `bitcode` 0.6, the version the workspace pins, checks each variant index against the enum's variant count as it decodes. A newer service emits the new events only for the new verbs, so an older client never receives one. For everything else, ADR-00022 still requires a client and service from the same build, and the version check [ADR-00025](../developing/decisions/00025-service-version-compatibility.md) proposes would refuse a mismatched pair. See Assumptions.
- **Client/server protocols** — No new RPCs, message types, fields, or authentication flows.
  - **New values in existing requests:** `branch_list` sets the request's existing `creator` and `include_deleted` fields, which today's client leaves unset and false (`lore-transport/src/grpc/revision_client.rs:116-125`). A remote that ignores `creator` returns every branch, and the verb filters the records itself. A remote that ignores `include_deleted` returns no archived branches.
  - **A standalone create:** `branch_create` runs on its own, outside a push, as layer and link creation already send it (`lore-revision/src/layer.rs:382-393`, `lore-revision/src/link.rs:383`).
  - **Older Lore Server:** The name claim lives in the code Lore Server shares with the client. Against a Lore Server without the change, the remote claim stays a plain write, and two concurrent creators can still both succeed there.
  - **Older clients:** Against an upgraded Lore Server, an older client whose create loses a concurrent claim on a name gets the status Lore Server already returns for a taken name: `ALREADY_EXISTS` through `lore.revision.v1` (`lore-server/src/grpc/revision/v1/branch_create.rs:179-186`), and `INVALID_ARGUMENT` through the deprecated `urc.rpc` (`lore-server/src/grpc/handlers/branch_create.rs:135-141`). `lore` clients of every version report both as `Internal` (`lore-transport/src/error.rs:30-49`).
  - **Push to an archived branch:** Lore Server's `BranchPush`, in `lore.revision.v1` and the deprecated `urc.rpc`, answers `NOT_FOUND` for an archived branch (see "Restores" under "Changes beneath the verbs"), where today it restores it. `lore` clients of every version create the branch under its id and push again (`lore-revision/src/branch/push.rs:1985-2009`). Another client creates the branch first.
  - **Archive keys:** `BranchMetadataSet` answers `INVALID_ARGUMENT` for a swap that adds, changes or removes `archived`, `restored`, `restoring` or `released`. `BranchDelete` and `BranchCreate` write them, and `BranchMetadataGet` returns them with the rest of the metadata.
  - **Create statuses:** Lore Server's `BranchCreate`, in `lore.revision.v1` and the deprecated `urc.rpc`, answers `INTERNAL` for a store failure and for any other failure but a taken name, a rejected argument or a store slowdown, where today each answers `INVALID_ARGUMENT` (`lore-server/src/grpc/revision/v1/branch_create.rs:196-204`, `lore-server/src/grpc/handlers/branch_create.rs:135-141`). An id that reads as zero, as an unset one or one shorter than 16 bytes does, answers `INVALID_ARGUMENT`, where today Lore Server creates a branch with id zero (`lore-server/src/grpc/revision/v1/branch_create.rs:141`, `lore-server/src/grpc/handlers/branch_create.rs:77`, `lore-base/src/types/mod.rs:442-457`). A client that generates the id, as `lore.revision.v1` asks (`lore-proto/proto/lore/revision/v1/revision.proto:33-42`), never sends one. A branch an older `lore` created with id zero, as `lore branch create` given a zero id does today (see "CLI and public API" below), is the exception: its first push, from any `lore` version, sends that id (`lore-revision/src/branch/push.rs:1081-1109`), an upgraded Lore Server refuses it, and `lore` reports the refusal as `Internal`. This proposal counts that as part of the breaking change to `lore branch create`. `lore` clients of every version report either status as `Internal` (`lore-transport/src/error.rs:30-49`).
- **On-disk format** — No format change. The verbs read and write the same mutable keys and branch metadata as the file-system verbs (`lore-revision/src/branch.rs:402-502`). The name claim changes how a writer sets the name-to-id key, not what the key holds. A restore sets a `restored` key in the branch metadata, and holds the name it claims in a `restoring` key until it finishes. A settle replaces that key with a `released` key, which the next restore removes. An archive sets an `archived` key, a time in milliseconds like `created`, and removes `restored`. An older version keeps metadata keys it doesn't know. Its restore of a branch this version archived maps the name and keeps `archived`, which this version reads as live. Its archive of a branch this version restored releases the name and keeps `restored`, which this version reads as archived. This version's next restore or archive of the branch settles or releases a `restoring` or `released` key it keeps, and releases the name. A restore by an older version under the name a `released` key names reads to this version as the name a settle left: this version's next archive of the branch releases it, and its next create given the branch's id releases it and restores the branch, renaming it where the create names another name, where it would otherwise fail with `BranchAlreadyExists`. A key a swap or store to zero removes leaves no entry or item behind (see "Removal on a swap to zero"). The local store writes a bucket a removal empties as a file with no entries, which an older version reads as an empty bucket (`lore-storage/src/local/mutable_store.rs:539-543`).
- **CLI and public API** — Additive, with behavior changes for existing verbs, five of them breaking.
  - **New functions in `lore-capi/lore.h`:** `lore_revision_graph_revision_resolve`, `lore_revision_graph_branch_resolve`, `lore_revision_graph_branch_list`, `lore_revision_graph_branch_create`, `lore_revision_graph_branch_archive`, `lore_revision_graph_branch_sync`, and `lore_revision_graph_branch_push`, each with a sync and an `_async` variant.
  - **New types:** `lore_revision_selector_t`, one args type per verb, `lore_revision_graph_branch_sync_options_t` and `lore_revision_graph_branch_push_options_t`, and the six event data types.
  - **Event tags:** Six new tags at the end of `lore_event_id_t`.
  - **Behavior changes:** The name claim, the removal on a swap to zero, the ancestry walk, and the refusal mapping change what existing verbs do in a race, over a key holding zero, on a failed read, without a remote, or on a rejected credential. Switch, reset and Lore Server's push no longer restore an archived branch, `lore branch metadata set`, `lore branch metadata clear` and `BranchMetadataSet` refuse the archive and restore metadata keys, and `lore branch create` and `lore_branch_create` report an invalid name, a zero id or an invalid parent as `InvalidArguments`:
    - Of two concurrent `lore_branch_create` or `lore branch create` calls for one name, one now fails with `BranchAlreadyExists` and removes its metadata key, where today both succeed and one branch loses its name. The loser still leaves branches in other repositories: it creates the branch in each linked repository before it claims the name, and in each layer after the claim fails. `branch_create` creates neither.
    - A `lore branch create`, `lore_branch_create` or Lore Server branch create now fails before it writes when it can't read the metadata of the branch id it's given or generates, a remote's refusal included, where today it writes fresh metadata over that branch. So a file-system create fails while the remote refuses the caller, where today it creates the branch locally. A create whose remote can't be reached still succeeds. A create also fails when it can't read the name's mapping in the local store, or the metadata of the branch the mapping names, where today it takes the name.
    - A create given an archived branch's id restores the branch under the new name when its old name now names another branch, where today it fails with `BranchAlreadyExists`. A lookup of the old name that fails other than as not found fails the create, where today it takes the old name as free.
    - A `lore branch create` or `lore_branch_create` given a name that isn't valid, or run on a personal branch, which can't have children, now fails with `InvalidArguments` (3), where today it fails as `Internal` (-1, exit status 255), as the command reports every failure of the create core but a taken name or a refusal (`lore-revision/src/branch/create.rs:248-249`). The command forwards the core's `InvalidArguments`, so it reports the code `branch_create` reports for the same argument (see "Errors"). One given a zero id fails with `InvalidArguments` as well, where today it creates a branch with id zero. This is a breaking change, and it covers the first push of a branch an older `lore` created with id zero (see "Create statuses").
    - Of concurrent creates that restore one archived branch, at most one succeeds and the others fail with `BranchAlreadyExists`, where today several can, which leaves the branch mapped from each name. A create that renames an archived branch without a latest revision fails with `BranchAlreadyExists`, where today it renames it: one whose create hasn't finished, or one without a latest revision that an earlier version archived, such as one made from an empty default branch.
    - A create that restores a branch records the name it claims in a `restoring` key before it claims it, and a create, archive or forced create that settles a stopped restore leaves the name in a `released` key. The next create or archive of the branch, or `lore branch create --force` of the name, releases a name either leaves, where a restore that stops part way today can leave the branch's metadata renamed and its name unmapped. Lore Server's branch create and delete run to their end, and still send their notification, run the post hook and update their counter, when the request's deadline passes or its client leaves, where today they can stop part way.
    - `lore branch archive` and Lore Server's branch delete read the branch's metadata again when a restore, a protect or a metadata edit changes it between the archive's read and its swap, and archive the branch as it then is, where today the archive releases the old name of a branch a concurrent restore renamed. `lore branch protect` and `lore branch unprotect` without a remote, and Lore Server's deprecated `urc.rpc` `BranchProtect` and `BranchUnprotect`, swap the metadata from the value they read, where today they store it whole and can put back metadata an archive or restore changed in between.
    - `lore branch metadata set` and `clear`, and Lore Server's `BranchMetadataSet`, refuse the `archived`, `restored`, `restoring` and `released` keys, which archives and restores own, and `BranchMetadataSet` refuses to add them. A value an application stored under one of those names before changes what restores and archives do: they take a `restoring` value for a stopped restore and settle it, as a name lookup and the branch metadata commands also do, release a name a `released` value holds, and a numeric `archived` or `restored` value changes a restore's checks and which names an archive or a failed restore releases. This is a breaking change.
    - A `lore branch switch` or `lore branch reset` onto an archived branch, given its id, leaves the branch archived, where today it maps the branch's name to it again, and with `--force` takes the name from another branch that holds it. A create with the branch's id restores it. A switch or reset given the id of a branch the remote holds live still maps its name locally, as a lookup by its name does. One without `--force` onto a branch whose name another branch holds goes ahead and leaves the name with that branch, where today it fails as `Internal`. This is a breaking change; "Keep switch and reset as restores" under Alternatives Considered says why only a create restores.
    - A clone that records a branch, and Lore Server's repair of a missing default-branch mapping, leave alone a name another writer claimed first, where today they overwrite it. A clone that can't read the mapping, or the metadata of the branch it names, fails.
    - `lore branch archive`, `lore branch create --force` replacing a branch, and Lore Server's branch delete leave alone a mapping that a concurrent writer pointed at another branch, where today they remove it. The forced create then fails with `BranchAlreadyExists`, and Lore Server's delete answers as it does for a branch deleted already, so of two concurrent deletes of one branch only one is counted, notified and runs the post hook. A forced create of a name that maps, in the local store, to a branch whose metadata neither names it nor records a restore under it, compared regardless of case, releases that mapping and takes the name, and leaves the branch as it is, where today it archives the branch under its own name and then fails.
    - Lore Server's push to an archived branch fails with `NOT_FOUND` and leaves the branch archived, as "Push to an archived branch" above describes. A name that maps to another branch with metadata now fails the push with `ALREADY_EXISTS`, as the create that restores the branch would, whatever name that metadata holds (`Feature`, created after `feature` was archived, or another name a restore under `feature` hasn't yet replaced), and one whose holder's metadata key can't be read fails it with `INTERNAL`, where today Lore Server takes the name in those cases. This is a breaking change for a client that relies on the push to restore the branch.
    - `lore_storage_mutable_list` no longer lists a key holding zero. Today the local store lists one with a zero value after a store or swap to zero, and after a swap from zero to zero. So `lore branch list` no longer asks the remote about each name archived in the local store.
    - A file-system push or sync whose ancestry walk has no remote to ask now fails with `RevisionNotFound`, naming the revision it couldn't read, where today the walk loops until a remote answers. A request the remote fails stops that line of the walk, and a walk that can then get no further fails with the remote's error, such as `Disconnected` or `NotAuthenticated`, where today a failed request reads as the end of the line. A revision lookup whose walk fails either way falls back to the local latest, as it already does on any walk error, instead of looping.
    - A call that gets an `UNAUTHENTICATED` status from Lore Server now reports `NotAuthenticated` instead of `Internal`, and the client treats the status as a refusal: it doesn't retry a failed storage session start with the same credentials (`lore-transport/src/connection.rs:350`), and a refused connect leaves the reconnect back-off (`lore-revision/src/protocol.rs:239`).
    - A storage query, fragment verification or mutable-store request, and a notification subscription, that Lore Server refuses as `UNAUTHENTICATED` or `PERMISSION_DENIED` now report `NotAuthenticated` or `NotAuthorized` instead of `Internal`, and a refused subscription fails at once where today a `PERMISSION_DENIED` one is retried. This refusal mapping shipped with change-request/1151.
  - **Unchanged:** No existing signature or event changes, no existing failure changes its code beyond the behavior changes above, and the CLI gains no commands.
- **Rust crate API** — Additive. The `lore` crate gains the `lore::revision_graph` module. Internal crates change in ways no public signature shows:
  - `ReadOptions` in `lore-storage` gains two fields, one for the verbs' reads at the remote and one for their reads in the local store.
  - The create and archive cores in `lore-revision` gain record writes, and the push core changes its signature.
  - The internal `Revision::branch_query`, `Revision::branch_list`, and `Revision::branch_create` transport methods in `lore-transport` change shape, which changes their implementations and test mocks.
- **Mutable store plugins** — A stricter contract on an existing interface. The `MutableStore` trait in `lore-storage` already requires a store of zero to remove the key. It now also requires what "Removal on a swap to zero" describes, and two things more of `compare_and_swap`: a key holding other than `expected` is left unchanged, so a swap from a non-zero value on an absent key fails and answers zero, where the trait counted an absent key as a match; and the answer is a value the key held while the call ran. The local and DynamoDB stores keep it. A mutable store Lore Server loads through its plugin factory (`lore-server/src/plugins/traits.rs:79-116`) that keeps a key holding zero leaves an entry per branch name behind after a branch or repository deletion, and lists a zero branch id unless its list leaves the key out. One that counts an absent key as a match reports a release or a metadata swap that didn't happen.
- **Metrics** — Lore Server's DynamoDB operation-latency metric times a compare-and-swap to zero as `delete_item` and one to the value a key holds as `get_item`, where it times both as `put_item` today, and its `delete_item` series gains a `conditional` label. Dashboards and alerts on these series see those swaps move.
- **Language bindings** — Additive. The JavaScript, Python, .NET, and Go bindings live in their own repositories (`README.md:112-120`) and gain the verbs by wrapping the new C functions. No existing binding changes.

## Non-Functional Considerations

- **Concurrency** — The proposal relies on compare-and-swaps that guard a branch's latest, and adds one on the name-to-id key and one on the branch metadata.
  - **Remote:** Concurrent pushes to one branch resolve through Lore Server's compare-and-swap loop on the remote latest (`lore-server/src/grpc/handlers/branch_push.rs:1286-1335`). At most one publishes, and each of the others fails with `BranchAdvanced` or lands through a fast-forward merge.
  - **Local:** A sync and a tree commit on the same store contend on the local latest's compare-and-swap (`lore-revision/src/branch.rs:871-903`), and one of them fails. `expected_local_latest` keeps a sync from overwriting a latest that changed since the caller read it.
  - **Shared store:** Calls on separate storage handles over one shared store share its local latest pointers and resolve through the same compare-and-swap.
  - **Names:** Every writer of a name-to-id key contends on the same compare-and-swap, so two creators of one name can't both win. The losing create fails with `BranchAlreadyExists`.
  - **Branch metadata:** Restores, archives and protects of one branch contend on a compare-and-swap of its metadata, as `BranchMetadataSet` already does. Of racing restores at most one succeeds, and an archive, or a protect without a remote, that loses reads the metadata again. A protect with a remote that loses fails, as a `BranchMetadataSet` swap that loses already does.
  - **Archive and instances:** The verb reads which instances stand on the branch before it archives anything, and archives it locally after, without the repository lock the instances' own commands take ([ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)). An instance that switches onto the branch in between ends up on an archived branch. See Risks.
- **Memory** — The proposal preserves Lore's streaming model, with the file-system verbs' existing buffering.
  - **List:** The transport buffers the `branch_list` response before the verb emits the remote's records (`lore-transport/src/grpc/revision_client.rs:132-151`), as the file-system list does. Records reach the caller one event at a time.
  - **Number resolution:** On the remote, the `revision_list` RPC. Locally, a walk of the branch's parent history, as the CLI does.
  - **Push:** Holds the hashes of the revisions to push and streams fragments, as `lore_branch_push` does.
  - **Archive and restore:** An archive serializes one branch metadata blob and a restore two, as a create serializes one. The first blob a process writes allocates the compression context, about 3.8 MB for a `lore branch archive`.
  - **Instance records:** A verb that reports them builds the list of registered instances once per call, one entry per instance over the store.
- **Statelessness** — The proposal introduces no process- or library-level state. Each call builds its contexts and drops them at return. The only state that outlives a call is state Lore already keeps: the storage handle's stores and storage session pools, and the per-partition remote connections.
- **Determinism** — Revision signatures stay a deterministic function of their content, as the revision tree proposal describes. Four outputs depend on more than their inputs, and the proposal accepts each:
  - **Latest:** Resolving "latest" answers with the latest at the time of the call.
  - **Branch ids:** `branch_create` generates a fresh branch id, as the file-system create does.
  - **Published revisions:** `branch_push` can publish a different revision than the one committed, through a fast-forward merge or a revision Lore Server re-serializes, and the pushed event reports the published revision.
  - **Instance records:** `revision` depends on what other repository instances have done, and `stale` on what the caller's process can see of the file system.
- **Latency** — Some answers cost reads beyond the main RPC:
  - **Remote branch records:** `protected` comes from each branch's metadata, one metadata read per branch.
  - **Instance records:** A verb that reads instance records lists the registered instances once per call and reads each one's current branch. For each instance on the branch it reads, or for `branch_list` each instance with a current branch, it reads the current revision and checks the registered path on disk for staleness. `branch_resolve`, `branch_list`, a local `branch_sync` with no target, and `branch_archive` when it archives locally all read them.
  - **Archive:** The checks read the branch's metadata and the repository's default branch, and the instance records when it archives locally.
  - **Restore and archive writes:** An archive writes one metadata blob, swaps the metadata key, and reads the key again before it releases the name. A restore writes two metadata blobs and swaps the key twice, around its claim. A settle, and a release of a `released` name, read the metadata again first, and the name lookup's and the branch metadata commands' caching of the remote's metadata reads the local metadata first; these run only where a restore stopped or was settled, apart from that read.
  - **Push:** A refused push costs one `branch_query` to confirm the refusal and report `remote_latest`, and one more, by name, when the branch was absent or archived on the remote before the push.
  - **Sessions:** A call that reads remote blocks under a `globals.correlation_id` no earlier call on the storage handle used starts one storage session per connection. A caller that keeps one correlation id pays that once.
  - **Mutable-store swaps:** In DynamoDB a compare-and-swap to the value a key holds is a consistent read, and one to zero a conditional delete, where both are conditional writes today.

## Migration Plan

Five changes break callers, one of them the refusal mapping, which shipped with change-request/1151. Lore Server and its clients upgrade in either order, and no migration rewrites stored data.

| Client | Lore Server | Push to an archived branch | Archive keys |
| --- | --- | --- | --- |
| `lore`, any version | This version | `NOT_FOUND`; the client restores the branch with a create and pushes again | `BranchMetadataSet` refuses them |
| Another client | This version | `NOT_FOUND`; the client restores the branch with a create first | `BranchMetadataSet` refuses them |
| This version | An older one | Lore Server maps the branch's name to it again | `lore branch metadata set` and `clear` refuse them; Lore Server accepts them |

- **Before the upgrade:** An application that stores its own value under `archived`, `restored`, `restoring` or `released` removes or renames it. After the upgrade, `lore branch metadata clear` and `BranchMetadataSet` refuse to remove it.
- **Scripts and C API callers:** A script that restores an archived branch with `lore branch switch` or `lore branch reset` given its id runs `lore branch create <name> --id <id>` as well, before or after the switch or reset. One that tests a refused request for exit status 255 or code `-1` tests for 16 or 17 as well, and one that tests a create given an invalid name, or run on a personal branch, for 255 or `-1` tests for 3 as well. One that creates a branch with a zero id passes another id or none, since that create now fails with 3, and the first push of a branch an older `lore` created with id zero fails against an upgraded Lore Server with `-1`, from any `lore` version: no version can push such a branch there.
- **Other Lore Server clients:** A client that sends `BranchCreate` with an id that reads as zero generates one, as `lore.revision.v1` asks (`lore-proto/proto/lore/revision/v1/revision.proto:33-42`); Lore Server now answers `INVALID_ARGUMENT` to such an id, through the deprecated `urc.rpc` as well.
- **Mutable store plugins:** A plugin changes its compare-and-swap and list to keep the contract "Mutable store plugins" names. Until it does, the only effect is the leftover entries and zero ids that item describes.
- **Stored data:** An entry or item holding zero that an older version wrote stays until a write removes it, and every load and list takes it as absent, so none needs a sweep. The new metadata keys stay, and older versions read them as On-disk format describes.
- **Rollback:** The signal is a non-`lore` client's push failing with `NOT_FOUND`, `BranchMetadataSet` answering `INVALID_ARGUMENT` for an application's own key, or `BranchCreate` answering `INVALID_ARGUMENT` for an id that reads as zero. Rolling Lore Server back brings back its restoring push, and leaves the metadata keys for older versions to read as On-disk format describes.

## Security Considerations

The proposal doesn't change the trust model.

- **Identity:** Identity and tokens come from `lore_global_args_t` and the storage handle, as for the storage and revision tree APIs (`lore-revision/src/protocol.rs:23-42`).
- **Remote writes:** Every write that reaches the remote goes through an RPC the file-system verbs use, so Lore Server applies the same checks: partition access on every RPC (`lore-server/src/grpc/tower/partition_access.rs:29-30, 162-194`), the push-protected permission and branch protection on push (`lore-server/src/grpc/handlers/branch_push.rs:826-848, 1031-1043`), the protected and default-branch refusals on delete (`lore-server/src/grpc/revision/v1/branch_delete.rs:150-177`), and revision verification on push (`lore-server/src/grpc/handlers/branch_push.rs:926-1015, 1228-1250`). A caller can't use this surface to push, create, or archive anything the same identity couldn't through the CLI.
- **Linked repositories:** The linked-revision check in `branch_push` reads the linked repository's partition under the caller's identity, so it reveals nothing that identity can't already read.
- **Local writes:** Locally, a verb writes only to the store the caller's storage handle opened. On a shared store the other repository instances use it too, but the caller can already write to it through the storage API.
- **Instance paths:** A verb that reads instance records (`branch_resolve`, `branch_list`, a local `branch_sync` with no target, and `branch_archive` when it archives locally) checks the registered path of each instance on the branch it reads, or for `branch_list` of each instance with a current branch, whether or not it reports the instance, reading `<path>/.lore/instance` (`lore-revision/src/instance.rs:857-886`). For a registration whose path is gone or holds no checkout, it also reads that instance's external directory under Lore's global data directory, where an instance served through a virtual file system keeps its `.lore` (`lore-revision/src/instance.rs:842-847`). Whoever can write to a shared store can therefore make a pipeline check arbitrary paths on its machine, and `stale` reveals what it found. On Windows, a UNC path makes the check contact the host it names. A shared store is local state the caller already trusts, and the CLI's `lore repository instance list` runs the same checks. Here every pipeline that resolves, lists, locally syncs, or archives branches on the store runs them, not only a user listing instances.
- **Crafted input:** The verbs reject a malformed selector before any read, and validate branch names before touching either location. Number resolution and listing walk a crafted repository with the same code the CLI already runs against it.
- **A malicious remote:** It can misreport a branch's latest to this surface exactly as it can to `lore_branch_list` and `lore_branch_push` today. The surface reports remote answers as remote answers and never substitutes them for local ones.
- **Sensitive data:** None new. The surface carries branch records and revision hashes the file-system verbs already return.

## Privacy Considerations

- **Data exposed:** Branch records expose `creator` and `created`, the same fields `lore_branch_list` returns today (`lore-revision/src/branch.rs:141-144`). They go to the same parties: the caller, and the remote that stores them. Instance records expose the ids, working-directory paths, and current revisions of the repository instances over the caller's store. That's the data `lore repository instance list` prints to anyone who can open the store, and it never leaves the machine.
- **New data:** Branch metadata gains the times of the branch's last archive and restore, in milliseconds, the name a restore in progress claims, and the name a settled restore had claimed. They're stored locally and in Lore Server and returned with the metadata, and carry no identity. The proposal adds no telemetry, logs no new identifiers, and sends no other new data to the remote.
- **Deletion:** Archiving keeps a branch's metadata, as the file-system archive does. That matches today's ability to delete or expire branch data. Archiving a branch now removes its name's mapping from the local store, and deleting a repository removes each branch's keys from it, where an entry holding zero, keyed by the key's salted hash, stayed before. DynamoDB already deleted the item.

## Risks and Assumptions

**Assumptions**

- **Assumption:** The cores this surface reuses run on a path-less context once the changes under "Changes beneath the verbs" are in place: the create's and the archive's record writes, branch listing, revision resolution with an explicit branch, and the push's history walk and upload (`lore-revision/src/branch.rs:1502-1758, 1934-2004, 2096-2123`, `lore-revision/src/revision.rs:1570-1708`, `lore-revision/src/branch/push.rs:922-1165`). — *invalidated if:* one of them needs repository instance state on a path this surface takes, beyond the current-branch reads it already skips when no instance exists.
- **Assumption:** Every mutable store Lore Server runs on offers a compare-and-swap on the name-to-id key, as the local and DynamoDB stores do for every key type (`lore-storage/src/local/mutable_store.rs:2181-2256`, `lore-aws/src/store/mutable_store.rs:110-139, 359-438`), and removes a key on a store or swap to zero, with a list that leaves out a key holding zero, as both do with "Removal on a swap to zero" in place. — *invalidated if:* a deployed mutable store lacks a conditional write for that key type, which would leave the race open there, keeps a key holding zero, which would leave an entry per branch name behind after a branch or repository deletion, or lists one, which would put a zero id in a branch list.
- **Assumption:** Pre-checks and confirming reads identify every refusal the error table names, and anything else surfaces as `Internal` with the remote's message. — *invalidated if:* a refusal the table names can only be told apart by the message text, which would need structured error detail on the wire and a new proposal.
- **Assumption:** `branch_create` never sends an existing branch id, because it generates a time-ordered UUID for each call. The `branch_create` RPC's documentation says a duplicate id fails with `ALREADY_EXISTS`, but Lore Server restores the branch with that id instead (`lore-proto/proto/lore/revision/v1/revision.proto:38-58`, `lore-revision/src/branch.rs:1572-1608`). — *invalidated if:* the generator repeats an id, which would restore another branch instead of failing.
- **Assumption:** Building contexts for every call costs little next to the call itself, because Lore caches the remote connection per partition. — *invalidated if:* profiling shows context setup dominating small calls, which would argue for the handle this proposal rejects.
- **Assumption:** A client and the Lore service it relays to come from the same build, as ADR-00022 requires, or the version check ADR-00025 proposes refuses the pair. — *invalidated if:* ADR-00025 isn't accepted and mismatched builds keep connecting. Appending keeps this proposal from shifting any existing command or event, but a later change that adds a variant ahead of the new ones would shift theirs.

**Risks**

- **Risk:** A create or archive on both locations succeeds on the remote and fails locally, which leaves the two disagreeing. — *mitigation:* The verb acts on the remote first, and its records name the locations it changed. `branch_sync` brings a remotely created branch into the local store, and an archive retried with the `local` flag completes the archive in the local store.
- **Risk:** A forced sync on a shared store discards revisions another repository instance committed and hasn't pushed. — *mitigation:* A caller on a shared store sets `expected_local_latest`, which limits the discard to a local latest the caller knows about.
- **Risk:** An automated caller force-pushes over other writers' history. — *mitigation:* `force` is opt-in per call, and Lore Server's branch protection still applies to every identity without the push-protected permission. Repositories that need stronger guarantees protect their branches.
- **Risk:** A caller that can't see the instances' working directories, such as a process in a container over a mounted shared store, sees every instance on a branch as stale. Such a caller's `branch_archive` then finds no live instance and archives a branch that instances stand on. — *mitigation:* The instance record still reports each registered instance on the branch and its current revision, and `stale` only says the path check failed from the caller's point of view. A caller that can't see the working directories reads the instance records before it archives. An instance left on an archived branch is in the state the last risk below describes, and recovers the same way.
- **Risk:** A caller fills in only part of a selector and selects something it didn't mean. — *mitigation:* A selector that isn't exactly one of the two forms fails with `InvalidArguments`, apart from `branch_sync`'s all-zero selector, which names the branch's latest.
- **Risk:** A repository instance switches onto a branch between the archive's instance check and its local archive, and ends up on an archived branch. — *mitigation:* The file-system archive already leaves another instance in that state, because it refuses only its own instance's current branch (`lore/src/branch.rs:1195-1209`). The instance keeps its current revision, and `lore branch create <name> --id <id>`, run in that instance, restores the branch.
- **Risk:** A restore, a settle, an archive, or a forced create releasing a mapping its holder's metadata doesn't account for, that stalls just before it releases a name, while another restore of the branch claims that name and finishes, can release the later restore's name. The branch is then live by its metadata with no name. An archive that swapped the metadata and is about to release that name then finds it released and fails with `BranchNotFound`, though the branch ends archived. — *mitigation:* Each reads the metadata again first. A failed restore and an archive release the name only where the metadata holds no `restoring` key for it and doesn't name the branch live under it; a settle, and any release of a `released` name, only while the metadata still marks the name released; a forced create only where the metadata neither names the branch under the name nor records a restore under it. The window is between that read and the release. Metadata with a `restored` key while its name doesn't map to the branch reads as archived, so a create with the branch's id restores it again.
- **Risk:** A name can stay mapped to a branch with no key naming it, in this order: (1) a restore records itself and stalls; (2) another settles it; (3) a later restore releases the `released` name and removes the key; (4) the stalled restore claims the name and stops before it finishes. Creates of the name then fail with `BranchAlreadyExists`, a restore under it fails the same way, and an archive releases only the branch's own name. — *mitigation:* The restore must stall across a settle and a later restore's release, and then stop, where a restore that goes on can't finish and releases its claim. The next restore or archive frees a settle that stops, and a claim that lands while the `released` key stays. `lore branch create --force` of the name releases a mapping the branch's metadata doesn't account for, and takes the name, leaving the branch live under its own.
- **Risk:** A name lookup or a branch metadata command, `get` included, that caches the remote's metadata settles a restore of the branch in progress in the same process, which then fails with `BranchAlreadyExists`. — *mitigation:* Only a caller that runs both at once in one process reaches it, between the restore's record and its finish, as a C API caller can, or two commands relayed to one Lore service: commands in separate processes that write to one store take its write lock in turn, and one without a write token caches nothing. The restore can be run again.

## Drawbacks

- Branch operations now have two public surfaces, the file-system verbs and this one, and a caller has to choose between them.
- Reporting one record per location leaves the caller to decide what a disagreement between the local latest and the remote latest means.
- Commit and push are two calls on two surfaces, so publishing a revision isn't atomic.
- `branch_sync` shares its name with `lore sync` but not its merge, so one name covers two different operations.

## Alternatives Considered

### Keep cloning

Callers keep doing what they do today: a clone or bare clone into a temporary directory for every branch-level step, through the file-system verbs.

*Rejected because:* each step writes repository state to disk. For the pipelines both proposals serve, the revision tree proposal finds that the working-tree route isn't slow for these callers but architecturally incompatible with what they do ([Motivation](2026-05-14-low-level-revision-api.md#motivation)). The clone also ties the caller to file-system verb semantics, such as the current branch, that it has no use for.

### Extend the revision tree handle

Add `branch_create` and `push` to `lore_revision_tree_*`, resolve a branch's latest with a load verb that takes a branch, and list branches from a handle loaded at the zero revision.

*Rejected because:* listing and resolving act on the repository, not on a tree. List would need a tree loaded only to stand in for its repository, and resolve would load a tree only to read its hash. The repository-level verbs the revision tree proposal deferred would need a home anyway, which would split branch operations across two namespaces.

### Let the file-system branch verbs run without a repository instance

Give `lore_branch_*` a mode in which a storage handle and a repository id replace `globals.repository_path`.

*Rejected because:* those verbs' meaning depends on the instance:

- List reports `is_current`.
- Create takes its parent from the current branch and switches the instance (`lore-revision/src/branch/create.rs:104-129, 216-221`).
- Push without a branch pushes the current one (`lore-revision/src/branch/push.rs:292, 512`).
- `switch`, the `merge_*` verbs, and `reset` act on a working tree.

An instance-free mode would give each verb its own exceptions and make the same verb behave differently depending on how the caller reached it.

### A stateful repository handle

`lore_revision_graph_open(store, repository)` returns a handle that every verb takes, with a matching `lore_revision_graph_close`.

*Rejected because:* a handle would hold nothing Lore doesn't already cache per partition, and it would bring a lifecycle to specify: close ordering against the storage handle, teardown when an IPC connection drops, and draining in-flight calls. Stateless verbs avoid all of it, and a later proposal can still add a handle-taking verb if one needs state across calls.

### Operate in a repository instance

Give every verb an optional instance id. Local operations would then act as that instance does: a branch's local current would be that instance's current revision, and a local sync would move that instance's current anchor.

*Rejected because:*

- For reads, the instance records already report every instance on a branch with its current revision, so an instance id would only filter what the caller can filter itself.
- For writes, an instance's current and staged anchors describe its working tree. Moving them from outside the instance leaves the working tree not matching the anchor, and races the instance's own commands. Those commands take the repository lock, and a storage handle never does ([ADR-00024](../developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)). Avoiding both means taking the instance's locks, respecting its staged state, and updating its working tree, which is the file-system API's job.
- Every verb would have two meanings depending on whether a caller passed an instance, the same objection the alternative of running the file-system verbs without an instance meets.

A virtual instance whose working tree derives from its current anchor, such as a clone served through a virtual file system, is the one case where moving an instance's current from outside would be a real sync. It needs its own contract with the instance's locks and staged state, and so a proposal of its own.

### Publish the mutable-key scheme and keep using the storage API

Document the branch key derivation, and let callers read and write branch state through `lore_storage_mutable_*`.

*Rejected because:*

- It would make an internal key layout a public contract, the same lock-in the revision tree proposal rejected for the node block format ([Expose the internal node block format directly](2026-05-14-low-level-revision-api.md#expose-the-internal-node-block-format-directly)).
- It still couldn't publish, because Lore Server refuses generic writes to branch latest pointers and metadata (`lore-server/src/protocol/storage/mutable_store_handler.rs:52-72`).
- It can't list remote branches, because `mutable_list` is local-only.

### Record a restore with one write over both keys

Swap a branch's metadata and its name's mapping in one conditional write, as DynamoDB's `TransactWriteItems` allows (`lore-aws/src/dynamodb.rs:539`), so a restore needs no `restoring` and `released` keys.

*Rejected because:* the `MutableStore` contract swaps one key at a time, and every store, plugin and remote store would need a swap over two. The local store would take two buckets under one lock, and a remote mutable store reaches Lore Server through the storage protocol's single-key swap, so a two-key swap would need a new protocol message, which the Non-Goals rule out.

### Keep switch and reset as restores

Let `lore branch switch` and `lore branch reset` onto an archived branch, given its id, restore it, claiming the name and recording the restore as a create does.

*Rejected because:* a create given the id already restores the branch, and `lore push` makes one on its own. A switch or reset that restores would repeat the create's record, claim and settle in two more places, and one that mapped the name while a create restored the branch under another name would leave it mapped from both.

### String revision identifiers

Accept `main@latest`, `main@25`, `main@<hash>`, and `~N`, as the CLI does.

*Rejected because:*

- An identifier without a branch means the branch the instance is on (`lore-revision/src/revision.rs:1765`), and there's no instance.
- A parsed string makes every caller round-trip through text.
- A structured selector says exactly what it names, and turning a string into one is the caller's choice.

## Prior Art

- **Git:** Git splits these operations the same way. [`git ls-remote`](https://git-scm.com/docs/git-ls-remote) lists a remote's refs without a clone, [`git rev-parse`](https://git-scm.com/docs/git-rev-parse) resolves a name to an object, [`git update-ref`](https://git-scm.com/docs/git-update-ref) moves a ref with an expected old value, and [`git push`](https://git-scm.com/docs/git-push) publishes. Each acts on a repository, not on a tree. Git keeps the remote's view as separate [remote-tracking branches](https://git-scm.com/book/en/v2/Git-Branching-Remote-Branches). Lore keeps one latest per branch per location, and this surface reports each location as its own record without adding a namespace of tracking pointers. [`git worktree list --porcelain`](https://git-scm.com/docs/git-worktree) reports each worktree's checked-out commit and branch, and marks one `prunable` when its directory is gone. Instance records and `stale` give the same view of a shared store's repository instances.
- **[libgit2](https://libgit2.org/):** It exposes the same operations without a working tree: [`git_branch_create`](https://libgit2.org/docs/reference/main/branch/git_branch_create.html), [`git_reference_lookup`](https://libgit2.org/docs/reference/main/refs/git_reference_lookup.html), [`git_remote_ls`](https://libgit2.org/docs/reference/main/remote/git_remote_ls.html), and [`git_remote_push`](https://libgit2.org/docs/reference/main/remote/git_remote_push.html). libgit2 opens a repository object first. This proposal does without one, because Lore's storage handle already plays that role.
- **[Jujutsu](https://jj-vcs.github.io/jj/latest/bookmarks/):** It shows a bookmark's local target beside each remote's target (`main` and `main@origin`) and leaves a conflict between them for the user to resolve. Records per location follow the same idea.
- **Perforce:** [`p4 changes -m1`](https://www.perforce.com/manuals/cmdref/Content/CmdRef/p4_changes.html) on a path finds the most recent submitted change without a workspace. Lore has had no equivalent for a branch's latest outside a repository instance.

## Unresolved Questions

None.

## Extensions

None yet. Verbs that amendments add under "Extending the surface" land here.
