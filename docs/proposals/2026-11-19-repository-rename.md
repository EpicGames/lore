---
lep: 2026-11-19-repository-rename
title: Repository rename
authors:
  - Hannes Muurinen
status: Accepted
created: 2026-11-19
updated: 2026-11-26
---

# Repository rename

## Summary

A repository on a remote has an ID and a name. The ID identifies the repository in every stored reference and in every request. The name is a string that a client resolves to the ID, for example when it clones. Lore has no operation that changes the name. This proposal adds a `RepositoryRename` RPC to `RepositoryService` and a `lore repository rename` command. A rename changes the name and keeps the ID, so revisions, branches, links, layers, permissions and repository instances are unaffected. A server that registers its repositories in an external registry asks the registry first, through a new `RepositoryRename` RPC on `RepositoryRegistryService`, and interrupts the rename process when the registry refuses. The server publishes a notification for each rename to notify event subscribers.

## Motivation

Repository names sometimes require changing: a project gets a new name, a repository moves to a different place in a deployment's naming scheme, or the name was wrong from the start. Lore has no way to change it:

- `name` is a read-only metadata key (`READ_ONLY_KEYS` in `lore-revision/src/metadata/repository.rs`), and `RepositoryMetadataSet` refuses a metadata blob that changes it.
- `RepositoryService` has no rename RPC (`lore-proto/proto/lore/repository/v1/repository.proto`), and the CLI has no rename command.

The only workaround is to create a repository with the new name, push the branches to it and delete the old one. The new repository has a new ID, and everything that refers to the old ID breaks:

- Links and layers in other repositories store the ID of the repository they refer to (`NodeLink` in `lore-revision/src/state/block.rs`, `Layer` in `lore-revision/src/layer.rs`).
- Each repository instance stores the ID, so every user has to clone again.
- Permissions are granted per repository ID, so the deployment has to grant them again.
- An external registry, and any other system that keeps data by repository ID, loses the association.

A rename also concerns two parties besides the Lore server:

- A deployment can register its repositories in an external registry (`RepositoryRegistryService`). The server sends the registry each repository's name at create. The registry can apply rules to names that Lore does not know, such as which names a caller may use. It has to be able to refuse a rename, and it has to learn the new name.
- Systems that depend on a repository, such as build automation or a review tool, can keep data by repository name. They have no way to learn that a name changed.

## Goals / Non-Goals

### Goals

- A caller who may administer a repository can change its name. The ID does not change.
- Repository instances, links, layers and permissions work after a rename without any action from their users.
- An external registry can refuse a rename before the server changes anything, and learns the new name of every rename.
- A rename that fails part-way leaves a state that the same rename, run again, completes. No failure requires manual repair of server data.
- Systems that depend on the repository name can observe a rename without polling.

### Non-Goals

- **Meaning in names.** Lore treats a name as one string. A name can contain `/` separators (`is_valid_name` in `lore-revision/src/repository.rs`), but Lore assigns no meaning to the parts, and a rename applies no rules to them.
- **Redirects from old names.** After a rename the old name is free, as it is after a delete. See [Old names](#old-names).
- **Races between operations on one name.** Two operations that take the same name at the same time can both succeed locally. Two concurrent creates have this exposure today, and rename does not remove it. See Risks.
- **Forwarded requests.** A server can forward `RepositoryCreate` and `RepositoryGet` to another server (`ForwardedRepositoryService`). It does not forward `RepositoryDelete`, and it does not forward `RepositoryRename`.

## Proposed Design

### The names handled by Lore server

A server keeps a repository's name in two places. The repository metadata contains the name, and is the primary source of truth. `RepositoryGet` by ID returns name directly from the metadata. There's also a name index in the mutable store that maps each name to an ID, and is used in lookups by name. Lookups already repair the index from the metadata: a lookup by ID writes a missing index entry, and a lookup by name removes an entry whose repository holds a different name (`lore-server/src/grpc/repository/v1/repository_get.rs`). A `RepositoryRegistryService`, when the server has one configured, can keep a third copy outside the server.

A rename changes all three. The design orders the changes so that a failure in any state is recoverable through a retry or a lookup.

### The RPCs

The proposal adds two RPCs: one that clients call, and one that the server calls on a registry.

A new RPC is added to `RepositoryService`:

```proto
// Rename a repository. The id does not change, and the previous name
// is freed. Renaming a repository to the name it already has succeeds,
// so a caller can complete an interrupted rename by sending it again.
rpc RepositoryRename(RepositoryRenameRequest) returns (RepositoryRenameResponse);

// Request to rename a repository by id. Names are not used here because
// they may have been recycled after a prior rename or delete.
message RepositoryRenameRequest {
  // Repository id.
  bytes id = 1;
  // The new name. Must satisfy the validation rules of
  // `RepositoryCreateRequest.name`, and must not be held by another repository.
  string name = 2;
}

// Response to a repository rename. Empty: the caller already holds the id
// and the new name.
message RepositoryRenameResponse {}
```

The request addresses the repository by ID, as `RepositoryDelete` does, because a name can refer to a different repository after an earlier rename or delete. The server finds the repository by its ID alone.

| Status | Condition |
|---|---|
| `INVALID_ARGUMENT` | The new name fails the rules that `RepositoryCreate` applies, or the registry's own name rules. |
| `NOT_FOUND` | The repository does not exist. |
| `UNAUTHENTICATED` | The registry rejects the caller's token, as for create and delete. The caller signs in again. |
| `PERMISSION_DENIED` | The caller may not administer the repository, or the registry refuses the caller or does not allow the name for this repository. |
| `ALREADY_EXISTS` | A different repository has the new name, on the server or in the registry, or the registry does not allow the name for this repository. |
| `ABORTED` | A concurrent operation changed the repository's name or took the new name after the checks passed. |
| `FAILED_PRECONDITION` | The registry does not know the repository. |
| `UNIMPLEMENTED` | The registry has no rename operation. |
| `UNAVAILABLE` | The server cannot reach the registry. |

A new RPC is added to `RepositoryRegistryService`, next to `RepositoryRegister` and `RepositoryUnregister`:

```proto
// Record the new name of a repository the Lore server is renaming. The Lore
// server calls this before it changes the name and refuses the rename when
// this fails, so the registry can authorize the rename. See
// `RepositoryRenameResponse`.
rpc RepositoryRename(RepositoryRenameRequest) returns (RepositoryRenameResponse);

// Request to rename a repository. The credential is the token the caller
// sent with the rename, forwarded by the Lore server: a token scoped to the
// repository when the client signed in over OIDC, and the login token
// otherwise, so the registry must accept either.
message RepositoryRenameRequest {
  // The repository id.
  bytes id = 1;
  // The new name.
  string name = 2;
}

// Response to a repository rename. Renaming a repository to the name the
// registry already holds for it succeeds for any caller allowed to rename
// it, so a repeated rename completes one the Lore server did not finish.
message RepositoryRenameResponse {}
```

[Registry](#registry) describes when the server calls the registry RPC and what it requires of it.

### Order of a rename

A rename has three phases:

1. **Local checks.** The server checks everything it can decide alone: the new name is valid, the repository exists, the caller may administer it, and the new name is not taken by another repository.
2. **Registry.** The server calls `RepositoryRename` on the registry, if one is configured. If the registry refuses or cannot be reached, the server returns the failure before changing anything.
3. **Local writes.** The server changes the name in the repository metadata and in the name index, then publishes the notification.

The design guarantees:

- **No validation after the registry.** Every check that depends on the request runs in phase 1. Once the registry has accepted, a rename fails only when the store fails or when a concurrent operation changed what phase 1 read.
- **The registry decides first.** The registry call is the first change a rename makes. A refusal leaves the server and the registry as they were.
- **Other metadata survives.** The rename changes only `name`. It installs the new metadata with a conditional write, so a concurrent `RepositoryMetadataSet` on another key, such as the description, is not lost.
- **No binding is overwritten.** The server binds the new name in the index only when the name is unbound or already bound to this repository, and changes the metadata only after that. If another repository took the name after phase 1, the rename returns `ABORTED` and the server's data is unchanged.
- **The old name stops at once.** From the moment the metadata holds the new name, a lookup by the old name answers `NOT_FOUND`, whether or not the rename has removed the old index entry yet.

### Failure after the registry accepted

The registry and the server's stores share no transaction, so a rename can stop after the registry accepted and before the local writes finish. The registry then holds the new name while the server still holds the old one, in whole or in part. The design accepts this state and makes it repairable by repetition:

- The caller sends the same rename again. The registry already holds the new name and accepts the call (see [Registry](#registry)). The server finishes whichever local writes are missing.
- A rename to the name the server already holds is not a no-op. The server still calls the registry and still repairs the name index, so the call also completes a rename whose metadata write succeeded but whose index writes did not.
- To abandon the rename instead, the caller renames the repository to its old name. The server still holds that name, so the call only moves the registry back. The registry decides whether it accepts the old name: a registry that reserves old names can still allow the repository that held the name to take it back.

The CLI reports a rename that failed after the checks passed as incomplete, and tells the user to run the command again.

### Registry

The server calls the registry's rename RPC from [The RPCs](#the-rpcs). It forwards the caller's token, as it does for `RepositoryUnregister`: a token scoped to the repository when the client signed in over OIDC, and the login token otherwise. The contract for the registry:

- Renaming a repository to the same name that the registry already has for this id succeeds for any caller allowed to rename it. The retry in [Failure after the registry accepted](#failure-after-the-registry-accepted) depends on this.
- The registry refuses a caller without permission to rename the repository with `PERMISSION_DENIED`, and a missing, invalid or expired token with `UNAUTHENTICATED`.
- The registry refuses a name it holds for a different repository with `ALREADY_EXISTS`. A registry that enforces unique names atomically resolves the race in Risks, because its answer comes before any local write.
- The registry refuses a name that fails its own name rules with `INVALID_ARGUMENT` and the `ErrorInfo` that `RepositoryRegister` documents in `lore-proto/proto/lore/user/v1/user.proto`: field `name`, and a reason of `ERROR_REASON_INVALID_FORMAT`, `ERROR_REASON_MISSING_REQUIRED_FIELD` or `ERROR_REASON_UNSPECIFIED`.
- The registry refuses a valid name that it does not allow for this repository, for example because it keeps the name reserved, with `ALREADY_EXISTS` or `PERMISSION_DENIED`.
- The registry answers `NOT_FOUND` for a repository ID it does not know. The server refuses the rename with `FAILED_PRECONDITION`, as it does for a create.

The server selects its registry from configuration (`RepositoryRegistry` in `lore-server/src/authnz/repository_registry.rs`):

- **No registry.** The rename has no external step.
- **`RepositoryRegistryService`.** The server calls `RepositoryRename`. A registry that does not serve the RPC answers `UNIMPLEMENTED`, and the server refuses the rename with `UNIMPLEMENTED`.
- **`RebacApi`.** This registry has no rename operation. The server refuses the rename with `UNIMPLEMENTED` before any write.

### Authorization

Rename uses the rule that `RepositoryDelete` uses (`repository_administration` in `lore-server/src/grpc/mod.rs`): a caller with the `owner` or `admin` permission may rename, and so may the repository's creator with the `write` permission. A registry can apply its own rules and refuse.

Rename has no permission name of its own. A rename breaks every reference to the repository by name, which makes it about as disruptive as a delete, so it takes the same permission. A separate permission can be added later without changing the RPC.

### Notification

The server publishes a notification for every `RepositoryRename` call that succeeds. A new payload is added to `lore.notification.Event`:

```proto
// An event published when a repository is renamed on the server
message RepositoryRenamed {
  // The name after the rename.
  string name = 1;
  // The name before the rename. Empty when the call found the new name
  // already in place.
  string previous_name = 2;
  // The user who renamed the repository.
  string user_id = 3;
}
```

The library reports renames as a new event, `LORE_EVENT_NOTIFICATION_REPOSITORY_RENAMED` in the C API. The CLI prints the new event in the `lore notification subscribe` event handler.

Delivery has the guarantees of the existing notifications. The server publishes after the local writes and does not store events. A repeated rename that finds the new name already in place publishes the event again, so a rename that stopped before the publish still notifies once the caller repeats it. The server cannot tell such a call from a repeat of a rename that already published, so a subscriber can receive the event more than once for one rename, and handles it idempotently. On a repeat the server no longer knows the previous name, and `previous_name` is empty. The notifications can be missed, if the subscriber wasn't registered as a listener at the time of the rename. A system that must not miss a rename can expect only best-effort messaging through notifications, and should query the names from Lore server by ID to get the authoritative answer.

### CLI and C API

`lore repository rename --name <new-name> [url]` renames a repository. The optional URL addresses the repository by name or by ID, in the forms that `lore repository delete` takes. Without it, the command renames the repository of the current repository instance, as `lore repository info` does. The client resolves a name to an ID and sends the ID. It sends an argument that parses as an ID as is, without a name lookup, as `lore repository delete` does. The C API gains `lore_repository_rename` and `lore_repository_rename_async`.

### Instances and references by ID

A rename does not change anything that refers to the repository by ID:

- A repository instance stores the repository ID in its Lore directory, and every request carries that ID (`inject_repository` in `lore-transport/src/grpc/mod.rs`).
- Links and layers store the ID of the repository they refer to.
- The server authorizes requests per repository ID.

Lore accepts an ID wherever it accepts a name. A clone URL can carry the ID in place of the name, and a lookup by name falls through to a lookup by ID when the name parses as an ID (`repository_load_name` in `lore-server/src/grpc/repository/v1/repository_get.rs`). Automation that must reach the same repository after a rename can use the ID.

### Old names

After a rename, the old name is free. A lookup by the old name returns `NOT_FOUND`, and a later create or rename can take the name. This matches how repository delete works at the moment. A registry can keep a tombstone for the old name and refuse another repository that tries to take it. See Alternatives Considered for redirects.

## Compatibility

- **Wire format** — Additive and backward compatible. The proposal adds new protobuf messages for the new RPCs and one new member, `RepositoryRenamed`, to the `Event.event` oneof. It changes no existing message, field or framing. An older peer never receives the new RPC messages, and it decodes an `Event` that carries the new member as an event with no payload. A newer peer decodes every message an older peer produces.
- **Client/server protocols** — Three additions:
  - `lore.repository.v1.RepositoryService.RepositoryRename`. An old client and a new server: no change. A new client and an old server: the server answers `UNIMPLEMENTED`, and the CLI reports that the remote cannot rename.
  - `lore.user.v1.RepositoryRegistryService.RepositoryRename`. The server calls it only during a rename, if the server is configured to use an external registry. A registry that does not implement the new RPC keeps working for create, delete and list, and renames fail with `UNIMPLEMENTED`.
  - `lore.notification.RepositoryRenamed` as member 11 of `Event.event`. An old client decodes the event with unknown payload and ignores it.
- **On-disk format** — N/A. A rename reuses existing metadata key and name index entries. An older server reads a renamed repository.
- **CLI and public API** — New `lore repository rename` subcommand. New `lore_repository_rename` and `lore_repository_rename_async` functions, and a new `LORE_EVENT_NOTIFICATION_REPOSITORY_RENAMED` event with its data struct, in `lore-capi`. No existing command or function changes.
- **Notification plugins** — A notification plugin (`notification.mode`) delivers the new event only once it implements it.

## Non-Functional Considerations

- **Concurrency** — The conditional write of the metadata orders a rename against other writers of the same repository: a concurrent change to another metadata key is kept, a delete that completes first makes the rename answer `NOT_FOUND`, and of two concurrent renames to different names one answers `ABORTED`. The conditional bind of the new name keeps a rename from overwriting another repository's name. Risks covers the exposure that remains.
- **Memory** — Preserved. A rename reads and writes one metadata blob.
- **Statelessness** — Preserved. All state is in the stores and the registry.
- **Determinism** — Preserved. Revisions refer to repositories by ID, so a rename does not change revision hashes.
- **Latency** — A rename makes one external call when the server has a registry, as create and delete do. This operation is not on a hot path.

## Migration Plan

N/A — no breaking changes, no migration required.

## Security Considerations

- **Trust model** — Unchanged. Rename is an administrative operation under the same rule as delete, and the registry can refuse it. On a server without token verification every caller is the same identity, so any caller with access can rename, as any caller can delete today.
- **Reuse of an old name** — After a rename, another repository can take the old name. Automation that resolves the old name on each run, such as a job that clones by name, then reaches a repository that the original owner does not control, and gets no error. With a registry, the registry decides who may take the name, and it can keep the old name from other repositories. Without one, any caller who may create a repository can take it. Referring to the repository by ID removes the risk. Existing cloned repository instances use the ID and are not affected.
- **Existence of names** — `ALREADY_EXISTS` tells the caller that some repository has the new name, as `RepositoryCreate` does today. The answer does not include that repository's ID.
- **Notification content** — The event carries both names and the user ID to every subscriber of the repository. A subscriber can already query the repository's name.

## Privacy Considerations

A rename removes the old name from the repository metadata and the name index, so it lets an administrator remove a name that should not be kept, such as one that contains a person's name. The old name stays in three places that the rename does not clear:
- the server log, which records repository names today
- the `RepositoryRenamed` event, which subscribers receive and can keep
- possibly the registry, which can keep its own history

The rename also does not delete the previous metadata blob, which holds the old name, from the immutable store.

## Risks and Assumptions

**Assumptions**

- **Assumption:** No server-side data other than the repository metadata, the name index and the registry is keyed by repository name. — *invalidated if:* a store, an authorization record or a server component looks data up by name and loses it after a rename.
- **Assumption:** A repository instance never resolves its repository by name after clone. — *invalidated if:* a command reads a name from the instance's stored remote URL and resolves it against the remote.
- **Assumption:** A registry can make `RepositoryRename` succeed when it already holds the requested name for the repository. — *invalidated if:* a registry implementation cannot tell a repeated rename from a conflicting one.

**Risks**

- **Risk:** A rename stops after the registry accepted and nobody runs it again, so the registry and the server hold different names. — *mitigation:* the CLI reports the rename as incomplete and suggests a retry. A repeated rename to the new name makes them agree, and so does a rename back to the old one that the registry accepts.
- **Risk:** Two renames of one repository to different names run at the same time. One completes and the other answers `ABORTED`, but the registry can hold the name of the call that answered `ABORTED`. — *mitigation:* accepted. A repeated rename of either makes the registry and the server agree.
- **Risk:** A repository create takes a name between a rename's check and its bind of that name. Create binds names with unconditional writes, so one of the two repositories can end up holding a name that the index maps to the other, and a lookup of that repository by ID then answers `NOT_FOUND`. — *mitigation:* accepted. Two concurrent creates have the same exposure today. A registry that enforces unique names atomically prevents it. `RepositoryRename` still reaches the affected repository by ID, so a caller who may rename it repairs it with `lore repository rename --name <new-name> <id>`.
- **Risk:** A dependent system misses the notification, because it was not subscribed when the server published it. — *mitigation:* the event is a hint, as the existing notifications are. The repository record queried by ID is the source of truth, and the dependent system must be built to reconcile the differences it detects, if missing a notification.
- **Risk:** A repository that the registry does not know, for example one created before the server had a registry, cannot be renamed. — *mitigation:* the operator registers the repository first when taking a registry service into use.

## Drawbacks

- An old name stops working at the rename, so every script and clone URL that uses it has to change. To prevent issues, the scripts can be changed to clone by ID before the rename.
- A registry has to implement its rename operation before the deployment can rename, and has to make it idempotent. Rename is a new feature, so this does not break anything existing.
- The registry and the server can disagree about a name until someone repeats a failed rename.
- A subscriber can receive `RepositoryRenamed` more than once for one rename, and without the previous name on a repeat.

## Alternatives Considered

### Change the server first, then the registry

The server renames locally and then tells the registry.

*Rejected because:* the registry could only refuse a rename that has already happened. The server would have to undo its own writes on a refusal, and a failure during the undo leaves a name that the registry refused in use. With the registry first, a refusal needs no undo.

### Roll back the registry when a local write fails

After a local failure, the server calls the registry again to restore the old name.

*Rejected because:* the rollback is a second external call that can fail for the same reason the local write did, so the state it is meant to prevent still occurs and now has two shapes. It also cannot run when the server process stops. A repeatable rename covers every case with one mechanism.

### Notify the registry through the notification stream only

The server adds no registry RPC, and the registry subscribes to `RepositoryRenamed` like any other system.

*Rejected because:* a subscriber cannot refuse a rename, so the registry could not enforce name rules or permissions, and it would miss renames published while it was not subscribed.

### A separate permission for rename

Rename gets its own permission name, so that a deployment can grant rename without granting delete.

*Rejected because:* a rename is about as disruptive as a delete, so the callers trusted with one are the callers trusted with the other, and a new permission name would require every token issuer to change. Adding the permission later is compatible with this design.

### Redirect the old name

A lookup by the old name answers the renamed repository, either permanently or until another repository takes the name, as GitHub does ([Renaming a repository](https://docs.github.com/en/repositories/creating-and-managing-repositories/renaming-a-repository)).

*Rejected because:* a permanent redirect reserves every name a repository ever had, which makes the name a second identifier and keeps a name in use that the rename was meant to retire. A redirect that ends when another repository takes the name moves the break to a future time chosen by the new repository creator. Ending the old name at the rename gives a `NOT_FOUND` at a time the repository administrator chose.

### Do nothing

Operators keep emulating renames by creating a new repository and copying the branches to it.

*Rejected because:* the new repository has a new ID, which breaks links, layers, cloned repository instances and permissions, as Motivation describes.

## Prior Art

Git hosting services treat the name as part of the address of a repository, so a rename has to keep old addresses working: GitHub redirects the old name until another repository takes it, and recommends that users update the remote URL of their clones ([Renaming a repository](https://docs.github.com/en/repositories/creating-and-managing-repositories/renaming-a-repository)). Lore's clients and stored references address a repository by ID, so a rename does not need the redirect to keep them working, and this proposal does not adopt it.

## Unresolved Questions

None.
