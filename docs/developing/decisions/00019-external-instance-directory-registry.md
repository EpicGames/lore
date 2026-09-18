---
status: proposed
date: 2026-07-28
---

# ADR-00018: External instance directory registry

## Context and Problem Statement

When using SWFS to virtualize the files in an instance of a Lore repository the instance's `.lore` directory needs to be
stored in some external location.

There needs to be some globally accessible registry of these external instance directories so that Lore can enumerate
them and look up whether a repository path has one.

## Decision Drivers

- It was already planned to store both the `.lore` and `write` directories required by SWFS-backed repositories in
  a directory `<Lore global data directory>/vfs/<instance ID>/`.
- The Lore service needs to enumerate all SWFS-backed repositories on startup in order to mount their repository path.
- If the Lore service has mounted a SWFS repository, a Lore client that isn't communicating with the service should
  still recognize it as a Lore repository and refuse to create a new repository there or operate on it (due to being
  SWFS-backed)
- Ideally these external instance directories should be usable by non-SWFS-backed repositories.

## Considered Options

1) Registry of shared stores with external instance information
    - Add a `registry.toml` to `<Lore global data directory>/stores` that has a record of all shared stores.
    - Require all SWFS-backed instances to use shared stores
2) Registry of stores (shared and some non-shared) with external instance information
    - Add a `registry.toml` to `<Lore global data directory>/stores` that has a record of all shared stores.
    - Also add all SWFS-backed instances' stores to the registry even if they're not shared.
3) Registry of external instance directories mapped to paths
    - `instances.toml` file stored in `<Lore global data directory>/instances` (renaming `vfs` to `instances` in the
      external instance directory path)

## Decision Outcome

Chosen option: "Registry of shared store with external instance information", because

- Requiring SWFS-backed instances to use a shared store aligns strongly with one of their primary use cases (multiple instances of the same repository on a machine).
  - A downside of using a shared store is that it puts the immutable store on whichever drive has the Lore global data directory.
    The alternatives require the immutable store to be on that drive as well, so this is unavoidable.
- It unifies user management of globally stored data.
  - Shared stores, SWFS instances, and external instance directories are globally stored data. They aren't cleaned up
    if a user deletes their repository directory.
  - Lore has basic tooling for creating/listing shared stores. This makes it so that those can be extended to
    "global data management" tools rather than requiring tools to be made specifically for SWFS and external instance
    directories.

## Pros and Cons of the Options

### 1) Registry of shared stores with external instance information

Add a `registry.toml` file to `<Lore global data directory>/stores`, which is where shared stores are created by
default. All shared stores (including those created outside `<Lore global data directory>/stores`) will be added to
`registry.toml` upon creation (or retroactively when a new repository uses them, for shared stores created before this
change).

Each Mutable Store already has information on each instance stored in it, accessible using the Repository ID. Each
shared store is only used by instances from one repository, so there is only one Repository ID associated with each
store. This means the registry of shared stores can also be used to enumerate all instances using shared stores.

By requiring SWFS-backed repositories to use a shared store, they will all implicitly be enumerable.

- Good, because a registry of shared stores was already planned for, the information stored in the Mutable Store already
  exists
- Bad, because all code that checks to see if a path is a Lore instance requires loading every shared Mutable Store and
  reading finding all instances associated with it to check if their path matches.
- Neutral, because requiring SWFS-backed repositories is a restriction, but one that lets us maintain a simpler golden
  path. This can also be changed later without too much extra work.

### 2) Registry of stores (shared and some non-shared) with external instance information

Similar to "Registry of shared stores with external instance information" except loosening the requirement that
SWFS-backed repositories use a shared store by putting their non-shared stores into the `registry.toml` file as well.

- Good, because it is more flexible for users than requiring SWFS-backed repositories use a shared store.
- Bad, because it turns `registry.toml` from an exhaustive list of shared stores to a non-exhaustive list of all stores.

### 3) Registry of external instance directories mapped to paths

Add an `instances.toml` file stored in `<Lore global data directory>/instances` that has the mapping between external
instance directories and their repository path.

- Good, because keeps external instance directory bookkeeping self-contained.
- Bad, because redundant with shared store registry bookkeeping that is planned to be done anyway.
