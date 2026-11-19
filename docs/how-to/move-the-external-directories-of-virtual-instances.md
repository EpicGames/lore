# Move the external directories of virtual instances

Each virtual instance keeps its Lore directory, and every file you change through its mount, in an external directory outside the mount. They all sit in one external root, `external/` in Lore's global data directory unless you move it. Use this guide to move them, to move them back, or to recover a lost external root. The move renames the external root, so the new location must be on the volume of the current one.

## Prerequisites

- A `lore` CLI that has `lore service set-external-directory`. See [Install the Lore CLI](install-lore-cli.md).
- On Linux and macOS, the nearest existing directory above the new location must belong to you or to root, and no one else may write it. The new location must not exist yet: Lore creates it, with access for you only.
- The new location must be on a file system Lore has verified as local and durable: `btrfs`, `xfs`, `bcachefs`, `ext2`, `ext3`, `ext4` or `zfs` on Linux, `apfs` on macOS, and a local drive on Windows.

## Before you move

Older Lore versions don't take part in the move:

- Stop any older Lore service, and close programs that run one.
- Upgrade every Lore client and program that links Lore: during the move, an older one fails every call it relays to the service.
- Keep the external root on a volume that's always mounted: an older `lore repository instance prune` retires a virtual instance whose external directory it can't find.
- On Windows, choose a location other users can't change, such as a directory in your user profile. Lore doesn't check who else can write there.

## Steps

1. **Check what the move would do.**

    ```bash
    lore service set-external-directory ~/work/lore-external --dry-run
    ```

    The dry run runs every check that needs nothing stopped, reports the service it would stop and the size of the external directories, and changes nothing. While a move is interrupted, it reports that move, which the command finishes or undoes first.

2. **Move the external directories.**

    ```bash
    lore service set-external-directory ~/work/lore-external
    ```

    The command stops the service, which unmounts your virtual instances, renames the external root, and starts the service again, which mounts them from there. A relative path is relative to your working directory. The stop is refused while a program is inside a virtual instance: close it, or add `--force`, which detaches busy mounts lazily, and programs with files open through them get errors. During the move, a command in a virtual instance fails. If a program holds one of your shared stores open, the command names the store and waits: close the program, or press Ctrl-C to undo the move. While it finishes an earlier interrupted move, Ctrl-C ends the command and leaves that move to the service.

3. **Check the result.**

    ```bash
    lore service status
    ```

    `External directories:` names the new location, and `State:` reads `can be used`. Lore leaves a link at `external/`, a symbolic link on Linux and macOS and a junction on Windows, through which older Lore versions find your virtual instances. On Windows they don't mount them.

## Move them back before a downgrade

Before you go back to an older Lore version, or run one as the service on Windows, move the external directories back:

```bash
lore service set-external-directory ""
```

Otherwise, an older Lore reports that a virtual instance's path isn't a repository wherever the link leads nowhere, such as while the external root's volume isn't mounted, and an older Windows service mounts none of them. Moving the external directories back with this version recovers them, but not a registration an older prune retired meanwhile.

## Recover a lost setting

Lore records the external root in `external-root` in its global data directory. If that file is lost, `lore service status` reports that the external directories can't be used. Point the setting at the external root again, or at a copy of it restored from a backup:

```bash
lore service set-external-directory ~/work/lore-external --adopt
```

`--adopt` moves nothing. It refuses a directory with no external root marker, and while `external-root` is there, one with the marker of another external root or of one given up with `--abandon`. On Linux and macOS it also refuses a directory you don't own or that others can write.

If an interrupted `--adopt` names a directory that has gone since, Lore can't finish it, and reports it. Run the command again with `--adopt` and another copy, or with `--abandon`.

## Give up an external root on a failed volume

If the volume that held the external root has failed permanently, switch to a new, empty external root:

```bash
lore service set-external-directory /mnt/new/lore-external --abandon
```

The virtual instances in the old external root are gone, and `lore repository instance prune` removes them. Lore doesn't use the old volume again if it comes back. `--abandon` refuses while the current external root can be used, and while `external-root` is missing: adopt the external root first.

## Replace a link you made by hand

Lore can't use a link it didn't make at `external/`. Stop the service, make sure no Lore command is running, and replace the link with the directory it names. On Linux:

```bash
lore service stop
rm ~/.local/share/lore/external
mv /mnt/work/lore-external ~/.local/share/lore/external
```

On macOS, the global data directory is `~/Library/Application Support/com.epicgames.lore`. On Windows, in a Command Prompt, where `rmdir` removes the link and not the directory it names:

```bat
lore service stop
rmdir "%LOCALAPPDATA%\Epic Games\lore\data\external"
move D:\lore-external "%LOCALAPPDATA%\Epic Games\lore\data\external"
```

`move` moves a directory only within its drive: copy one from another drive with File Explorer, then delete the original. Once the directory is in place, move the external root with `lore service set-external-directory`, within the volume of the global data directory.

## Result

`lore service status` names the new external root, and your virtual instances are mounted from it.

## See also

- [`lore service set-external-directory` in the CLI command reference](../reference/lore-cli-commands.md#lore-service-set-external-directory) — the command and its options.
- [Use virtual instances on Linux](use-virtual-instances-on-linux.md) — make, move and remove virtual instances.
