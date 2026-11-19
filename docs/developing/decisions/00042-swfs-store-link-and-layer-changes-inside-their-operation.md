---
status: accepted
date: 2026-11-13
---

# ADR-00042: Store a link or layer change inside the operation that makes it

## Context and Problem Statement

An SWFS instance serves its links and layers through its served tree, loaded from what the instance has stored: each link at the pin the stored staged state holds, each layer at the revision its stored configuration holds (ADR-00036). An operation's finalize describes to SWFS each link or layer that differs from what the mount last described, by its mount path (ADR-00037).

`link add`, `link update`, `link remove`, `layer add` and `layer remove` stored their change after the operation that wrote the working tree had finalized. That finalize found the old served tree. The linked or layered files the operation had written were not served from their own repository, so they were realized on the mount as local changes SWFS holds (ADR-00033), and the link or layer itself was described only by a later operation. Unstaging a link's repin left the mount serving the unstaged pin the same way.

How should a link or layer change reach the operation's finalize?

## Decision Drivers

- The thaw that ends a command's operation must describe the link and layer changes that command made, so the working tree after the command is the one a plain instance would hold.
- Linked and layered files must stay virtual, not become local changes in SWFS's write cache.
- The provider interface should not carry SWFS-specific state hand-overs. The [LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md) removed the `hold_state`, `stored` and `committed` hooks for this reason.

## Considered Options

- Store the change inside the operation, before it finalizes.
- Hand the new staged state or layer configuration to the operation, for its finalize to load instead of what is stored.
- Store the change after the operation, then run a second operation to describe it.

## Decision Outcome

Chosen option: "store the change inside the operation", because finalize already loads what the instance has stored, as a sync already stores its anchor inside its operation. Handing the state to the operation adds to the provider interface the hand-over the LEP removed. A second operation costs a second freeze and thaw per command, and between the two the mount serves the command's files as local changes.

Each command that changes a link or layer runs its working-tree changes and the store in one operation (`with_operation`), and stores before the operation finalizes:

- `link add`: the mount point, the staging, the clone of the link, and the staged state.
- `link update`: the realization of the new pin, and the staged state.
- `link remove`: the check for local changes, the removal from the mount, and the staged state.
- `layer add`: the mount directory, the clone under it, and the layer configuration.
- `layer remove`: the walk for local changes, the removal, and the configuration without the layer.
- Unstaging: the unstaged paths and the staged state that remains, so a link whose repin is unstaged is served at its old pin.

### Consequences

- Good, because the thaw ending a command's operation describes the link and layer changes it made, so their files stay virtual.
- Good, because the provider interface takes no SWFS-specific hand-over.
- Bad, because a command that fails after storing its change, inside the operation, can leave the staged state or the layer configuration ahead of the working tree, as a sync can already.
- Neutral, because each command holds its operation, and so the mount's operation lock, for the store as well.

## More Information

Related: ADR-00033 (results the revision does not hold are written to the mount), ADR-00036 (serving links and layers through the served tree), ADR-00037 (describing link and layer changes by their mount paths).
