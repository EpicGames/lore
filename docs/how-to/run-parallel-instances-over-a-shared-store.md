# Run parallel instances over a shared store

A [shared store](../glossary.md#shared-store) keeps a repository's content once on your machine for any number of working trees. Each working tree over the store is an [instance](../glossary.md#repository-instance): a clone with its own working tree, view, current branch, and staged state. A fragment fetched for one instance is there for every other, and it takes disk space once.

In this guide, you'll set up a shared store, add an instance for each branch, build, or automated tool you run in parallel, and keep the instances in order as you move and remove them.

If you've used Git worktrees, instances cover the same ground without a main repository:

- **No instance owns the store.** Delete any instance, the first one you cloned included, and the others keep working.
- **Every instance is a clone.** You create each one with `lore clone`, or copy one you have with `lore repository instance clone`. No instance has to exist before the others.
- **Branches and revisions live in the store.** A revision committed in one instance is in every other instance at once, before you push it, and it stays after you delete the instance that committed it.
- **The store outlives its instances.** Deleting the last instance leaves the store in place, and the next clone uses it again.

## Prerequisites

- The `lore` CLI on your PATH. See [Install the Lore CLI](install-lore-cli.md).
- A repository on a Lore Server. The examples use `my-project` on the server from the [Quickstart](../tutorials/quickstart.md), at `lore://127.0.0.1:41337/my-project`.

## Set up the shared store

1. **Turn on automatic use.**

    ```bash
    lore shared-store set-use-automatically true
    ```

    Every `lore clone` and `lore repository create` on this machine then uses a shared store. To opt in one clone at a time instead, leave the setting off and pass `--use-shared-store` to each clone.

2. **Clone the first instance.**

    ```bash
    cd ~
    lore clone lore://127.0.0.1:41337/my-project my-project-a
    ```

    The first clone from a server finds no store for it and creates one:

    ```text
    [Warn] Creating new shared store for remote lore://127.0.0.1:41337 in shared store at /home/you/.local/share/lore/shared_store/127.0.0.1_41337/shared_store
    Cloning repository 3f2a1b4c5d6e7f8a... branch main into /home/you/my-project-a
    ...
    Clone complete in 0.12s
    ```

    The instance's `.lore/` holds only its config and identity. Its content lives in the store.

3. **Check the store.**

    ```bash
    lore shared-store info
    ```

    ```text
    Shared store will be used automatically: true
    Remote URL: 127.0.0.1:41337
      Path: /home/you/.local/share/lore/shared_store/127.0.0.1_41337/shared_store
      Exists: true
    ```

    Linux is shown. The store lives in Lore's data directory, which is `~/Library/Application Support/com.epicgames.lore` on macOS and `%LOCALAPPDATA%\Epic Games\lore\data` on Windows.

One store serves every repository on a Lore Server, so a clone of another repository from `lore://127.0.0.1:41337` uses the same store.

> [!NOTE]
> A working tree that keeps its own store in `.lore/`, such as `~/my-project` from the Quickstart, stays on it. To bring a repository onto the shared store, clone a new instance of it.

### Keep the store on another drive

To keep the store somewhere other than Lore's data directory, create it there before the first clone. Pass the server's address, with no repository path:

```bash
lore shared-store create lore://127.0.0.1:41337 --path /mnt/data/lore
```

Lore creates the store in a subdirectory named after the server, `/mnt/data/lore/127.0.0.1_41337/shared_store`, and makes it the default store for that server, so later clones use it.

> [!CAUTION]
> Create the store before you clone any instance from the server. A store created later becomes the new default, and every instance that follows the default then opens the new, empty store and fails with `anchor branch is missing`.

## Add an instance

Clone the repository again for each branch, build, or automated tool you want to run in parallel:

```bash
lore clone lore://127.0.0.1:41337/my-project my-project-b
```

The clone fetches only what the store doesn't already hold. Every `lore clone` option works the same over a shared store: `--branch` starts the instance on another branch, and `--view` gives it a sparse view of its own.

### Clone an instance you already have

Each instance writes every file its view selects, so a full working tree costs its full size on disk again. To copy an instance instead, run from it:

```bash
lore repository instance clone ~/my-project-c
```

The clone is an instance of its own, on the source's branch, revision, and staged state, with the source's uncommitted changes. On the source's volume, on a filesystem that clones files (APFS on macOS, Btrfs, XFS with reflink or bcachefs on Linux, Dev Drive or ReFS on Windows), it shares the source's data blocks and takes little space. Its first `lore status --scan` hashes only the files a scan of the source would: those with no [modified-time record](../glossary.md#modified-time-record), and those changed since theirs.

The clone first waits for Lore commands already running on the source instance in its process to finish. While it runs, Lore commands on the source instance wait for it, and so do Lore commands from other processes on every instance over the store. `lore repository instance delete` waits too, since it opens the store to deregister the source before it removes any file. `lore repository instance move` doesn't wait: it renames the source before it opens the store, so moving the source while the clone copies it makes the clone fail. On Windows the move is refused while the clone lists a directory of the source. Commands that don't open the store, such as `lore repository config get`, don't wait either. Where the Lore service runs your commands, it runs them in its own process with the clone, so commands it runs on other instances over the store don't wait.

A repository nested inside the instance is an instance of its own, and the clone leaves it out. An instance that keeps its own store in `.lore/`, and a virtual clone, can't be cloned this way, and the destination can't be inside an instance, the source included.

Elsewhere, such as on NTFS, on ext4, or on another volume, the clone is refused, and the message says what a full copy would cost. Pass `--allow-copy` to make the copy anyway. A copy writes every file and takes as long as that does. Run with `--dry-run` first to see whether a clone shares blocks. Where it can't, the dry run says what a copy would cost and is refused as the clone would be; add `--allow-copy` to see the copy it would make.

## Work across instances

### Use a branch from another instance

A branch you create and commit to in one instance is in every instance at once, before you push it. Switch to it from any other instance:

```bash
lore branch switch feature
```

```text
Switching branch to feature, using current local latest revision b4e9f0a2...
```

Any instance can push the branch's revisions.

### Keep one instance per branch

When you switch to a branch that another instance has checked out, Lore warns you and names that instance:

```text
Warning: Branch 9a1c4b7d... is checked out in other instance(s):
  (01a134f3df717142...) /home/you/my-project-b
```

Two instances on one branch share its latest revision. Once one of them commits, a commit in the other fails with `Branch has been advanced by another instance, sync and re-stage to commit`. To commit on top, take your changes off the stage with `lore unstage`, sync, and stage them again:

```bash
lore unstage .
lore sync
lore stage <paths>
lore commit "<message>"
```

`lore unstage` keeps your changes in the working tree.

## List the instances

From any instance of the repository:

```bash
lore repository instance list
```

```text
01a134f3df2b7f50... /home/you/my-project-a main a3f8c2d1...
01a134f3df717142... /home/you/my-project-b feature b4e9f0a2...
```

Each line gives the instance ID, its path, its branch, and its revision. A line ending in `(stale)` names an instance whose directory is gone, and one ending in `(no checkout)` an instance whose directory no longer holds it. Pruning removes both.

To list every shared store on the machine, with the instances of every repository over each:

```bash
lore shared-store list --include-instances true
```

```text
Store at path: /home/you/.local/share/lore/shared_store/127.0.0.1_41337
  Remote URL: lore://127.0.0.1:41337
  Instances:
    -01a134f3df2b7f50...: /home/you/my-project-a
    -01a134f3df717142...: /home/you/my-project-b
```

`--include-instances` takes a value, `true` or `false`.

## Remove an instance

1. **Commit what you want to keep.**

    Revisions you've committed stay in the store, on their branch, after the instance is gone. Changes you haven't committed are deleted with the directory.

2. **Delete the directory.**

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    rm -rf ~/my-project-b
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    Remove-Item -Recurse -Force ~/my-project-b
    ```

    <!-- tabs:end -->

3. **Prune the instance list.**

    From any other instance of the repository:

    ```bash
    lore repository instance prune
    ```

    ```text
      Pruned 01a134f3df717142... /home/you/my-project-b (stale)
    Pruned 1 stale instance(s)
    ```

    Every clone over the store also prunes the stale instances of its repository, so the list catches up at the next clone if you skip this step.

## Move an instance

1. **Move the directory.**

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    mv ~/my-project-a ~/work/my-project-a
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    Move-Item ~/my-project-a ~/work/my-project-a
    ```

    <!-- tabs:end -->

2. **Record the new path.**

    In the moved instance:

    ```bash
    cd ~/work/my-project-a
    lore repository update-path
    ```

> [!WARNING]
> Run `lore repository update-path` before any other instance prunes or clones. Until you do, the instance list records the old path, which no longer exists. A prune, including the one every clone over the store runs, takes the moved instance for a deleted one and discards its branch, revision, and staged state. The instance then fails with `anchor branch is missing`, and you have to delete it and clone it again. The revisions it committed stay in the store.

## Free disk space

The store keeps itself within the limits in the `[store]` table of the instance's `.lore/config.toml`, 10 GiB by default. Past a limit, Lore evicts content the Lore Server also holds and fetches it again when it's next read. It never evicts revisions you haven't pushed. See the [`[store]` table](../reference/lore-cli-config.md#store-table) for the limits and how to change them.

To delete a store and everything in it:

1. **Remove every instance over it.**

    `lore shared-store list --include-instances true` lists them, for every repository on the server. Remove each one as in [Remove an instance](#remove-an-instance).

2. **Delete the store directory.**

    Delete the path `lore shared-store info` reports for the server. The next clone from that server creates a new, empty store in the same place.

> [!WARNING]
> Deleting a store deletes every revision committed in its instances and not pushed, on every branch of every repository over it.

## Turn off automatic use

```bash
lore shared-store set-use-automatically false
```

New clones then keep their own store in `.lore/`. Instances already over the shared store keep using it. While automatic use is on, you can only clone a repository without the shared store by turning the setting off first.

## See also

- [Quickstart](../tutorials/quickstart.md) — create a repository, then clone a second working tree over a shared store in Step 6.
- [Lore CLI command reference](../reference/lore-cli-commands.md#lore-shared-store) — every `lore shared-store`, `lore repository instance`, and `lore clone` flag.
- [Lore CLI configuration reference](../reference/lore-cli-config.md#shared-store-table) — the `[shared_store_to_use]` and `[store]` tables in an instance's config.
- [Shared stores and instances](../explanation/system-design.md#24-shared-stores-and-instances) — why instances are peers with no main repository.
