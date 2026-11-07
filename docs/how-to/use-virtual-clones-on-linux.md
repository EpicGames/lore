# Use virtual clones on Linux

A virtual clone is a working tree that the Lore service mounts with FUSE and serves from the store. Cloning one writes no files, a file's content is fetched the first time it's read, and every change is recorded as it's written, so `lore status` needs no `--scan`. Reach for one when a repository is too large to write out in full, or when you only touch part of it. This guide assumes you can already clone the repository, and covers making virtual clones, getting them back after the service stops, and moving and removing them. For a step-by-step walk through the edit loop in one, follow [Work in a virtual clone on Linux](../tutorials/work-in-a-virtual-clone.md).

## Prerequisites

- Linux 6.8 or later, with `/dev/fuse` and `fusermount3`. Most distributions ship `fusermount3` in a package named `fuse3`.
- The `lore` CLI on your PATH. See [Install the Lore CLI](install-lore-cli.md).

## Make a virtual clone

1. **Name the build that serves your clones (optional).**

    Every command that makes or works in a virtual clone is carried out by the Lore service. A clone that finds no service running starts it from the `lore` you ran. If several builds of `lore` are on your machine, name the one that serves your clones, so it doesn't depend on which you run first:

    ```bash
    lore service set-executable "$(command -v lore)"
    ```

2. **Choose where the shared store lives (optional).**

    A virtual clone keeps the content it fetches in the shared store for its server, and the service finds its virtual clones through the shared stores when it starts. The first clone from a server creates its store in Lore's data directory, `~/.local/share/lore`. To keep the store on another drive, create it there before you clone anything from that server, passing the server's address with no repository path:

    ```bash
    lore shared-store create lore://127.0.0.1:41337 --path /mnt/data/lore
    ```

    A store created after the first clone becomes the new default, and clones that follow the default then open the new, empty store. See [Keep the store on another drive](run-parallel-instances-over-a-shared-store.md#keep-the-store-on-another-drive).

3. **Clone with `--vfs fuse`.**

    ```bash
    lore clone lore://127.0.0.1:41337/my-project ~/my-project-fuse --vfs fuse
    ```

    `findmnt ~/my-project-fuse` then lists a `fuse.lore` mount. `--vfs default` makes the same kind of clone on Linux. A virtual clone always uses the shared store, so it needs no `--use-shared-store`.

4. **Work in it as in any working tree.**

    Edit files with any tool. The mount records each change as it's written, so `lore status` and `lore stage .` see it with no `--scan` and no `lore file dirty`. Stage, commit, push, sync, and switch branches as usual.

## Get your clones back after the service stops

The service holds the mount of every virtual clone it serves. Once it stops, after `lore service stop` or a restart of the machine, each clone's directory is empty, and a command there reports `Repository not found` rather than start the service. Start the service to mount every virtual clone it finds through your shared stores again, at the revision each one stood on and with your uncommitted changes in place:

```bash
lore service start
```

## Move a virtual clone

Run the move from outside the clone: the service can't move a mount that a shell or a program has as its working directory.

```bash
lore --repository ~/my-project-fuse repository instance move ~/work/my-project-fuse
```

The service mounts the clone at the new path, with its uncommitted changes, and removes the old directory. It creates the directories above the new path that don't exist yet, here `~/work`, and removes them again if the move fails. The new path itself must not exist.

## Remove a virtual clone

Removing a virtual clone deletes its uncommitted changes, so commit and push what you want to keep first. Run the removal from outside the clone, as for a move:

```bash
lore --repository ~/work/my-project-fuse repository instance delete
```

The service takes the mount down, deregisters the clone, and removes its directory.

## Stop the service

```bash
cd ~
lore service stop
```

A stop is refused while a shell or a program is inside a virtual clone. Leave the clone and close the program, or run `lore service stop --force`, which detaches every mount at once and cuts off the programs inside.

## Result

Your virtual clones are mounted while the service runs, and `lore status` in one lists your changes with no `--scan`.

## See also

- [Work in a virtual clone on Linux](../tutorials/work-in-a-virtual-clone.md) — the edit loop step by step, and what to do when a clone fails.
- [`[service]` table in the CLI configuration reference](../reference/lore-cli-config.md#service-table) — which build is started as the service.
- [`lore clone` in the CLI command reference](../reference/lore-cli-commands.md#lore-clone) — `--vfs` and the other clone flags.
- [Run parallel instances over a shared store](run-parallel-instances-over-a-shared-store.md) — shared stores and the instances over them.
