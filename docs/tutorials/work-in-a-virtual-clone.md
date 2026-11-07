# Work in a virtual clone on Linux

In this tutorial you'll clone a repository as a virtual clone, a mount that the Lore service serves straight from the shared store, and run the everyday edit loop in it: edit files, check status, commit, push, sync, switch branches, reset an edit and merge a branch. When you're finished, your virtual clone will hold the same files as an ordinary working tree of the same repository, and you'll know how to get it back after the service stops or a clone fails.

A virtual clone writes no files when you clone it. Every read of tracked content is answered from the store, which fetches what it doesn't hold on first read, and only the files you create or change are written to disk, outside the mount. Every command you run in a virtual clone is carried out by the Lore service that serves its mount, and a clone that finds no service running starts it.

## Prerequisites

- Linux 6.8 or later, with `/dev/fuse` and `fusermount3`. Most distributions ship `fusermount3` in a package named `fuse3`.
- A nightly build of `lore` and `loreserver`. See [install the Lore CLI](../how-to/install-lore-cli.md).
- The local server, the `~/my-project` repository and the shared store from the [quickstart](quickstart.md), Steps 1 to 9. This tutorial picks up where it ends: `main` at revision 3, holding `hello.txt`, `notes.txt` and `sample.bin`.

## Steps

1. **Start the Lore service.**

   Name the build that serves your commands, then start it. The [background service tutorial](run-commands-through-the-service.md) explains both commands. This step is optional: the clone in the next step starts the service when none is running, from the `lore` you run.

   ```bash
   lore service set-executable "$(command -v lore)"
   lore service start
   ```

   ```text
   Lore service executable set to /usr/local/bin/lore
   Lore service is running
   ```

   (Your path will differ.)

2. **Clone the repository as a virtual clone.**

   `--vfs fuse` makes the clone a virtual one, over the shared store from the quickstart. It always goes to the service, whether or not you've turned on running commands through the service.

   ```bash
   cd ~
   lore clone lore://127.0.0.1:41337/my-project my-project-fuse --vfs fuse
   ```

   ```text
   Cloning repository 3f2a1b4c5d6e7f8a... branch main into /home/you/my-project-fuse
   Cloned 0/0 files (0 bytes/0 bytes)
   Branch main revision c7d1e8b3...
   Clone complete in 0.02s
   ```

   `Cloned 0/0 files` is the point: nothing was written. The files are there all the same:

   ```bash
   findmnt ~/my-project-fuse
   ls ~/my-project-fuse
   ```

   ```text
   TARGET                    SOURCE           FSTYPE    OPTIONS
   /home/you/my-project-fuse lore:01a0f9ac... fuse.lore rw,nosuid,nodev,noatime,...
   hello.txt
   notes.txt
   sample.bin
   ```

   The mount's source names only the instance id, never the repository, its address or your user.

3. **Edit, create and delete files.**

   Work in the clone with any tool, as in an ordinary working tree.

   ```bash
   cd ~/my-project-fuse
   echo 'Hello from a virtual clone' > hello.txt
   echo 'Written through the mount.' > todo.txt
   rm sample.bin
   ```

4. **Check the status.**

   ```bash
   lore status
   ```

   ```text
   Repository 3f2a1b4c5d6e7f8a...
   On branch main revision 3 -> c7d1e8b3...
   Remote revision 3 -> c7d1e8b3...
   Local branch in sync with remote
   Changes not staged for commit:
   M hello.txt
   D sample.bin
   Untracked files:
   A todo.txt
   ```

   You didn't pass `--scan` or run `lore dirty`. The mount records every change as it happens, so status already holds every write whose system call returned.

5. **Stage, commit and push.**

   ```bash
   lore stage .
   lore commit 'Edit through the mount'
   lore push
   ```

   ```text
   Staging file system changes
   Staging 3 files (1 modified, 1 added, 1 deleted, 0 moved)
   ...
   Revision  : 4
   ...
       Edit through the mount
   Commit succeeded
   ...
   Pushed revision 4 -> 5c680a5f... to branch main
   ```

   The committed files are served from the store again, each keeping the file number `stat` reports and its modification time, so an editor holding one open notices nothing.

6. **Sync a change made in your ordinary working tree.**

   Bring `~/my-project` up to date, add a file there and push it:

   ```bash
   cd ~/my-project
   lore sync
   echo 'Written in the ordinary working tree.' > ordinary.txt
   lore stage ordinary.txt
   lore commit 'Add ordinary.txt'
   lore push
   ```

   Then sync the virtual clone:

   ```bash
   cd ~/my-project-fuse
   lore sync
   ls
   ```

   ```text
   Sync from remote lore://127.0.0.1:41337
   On branch main revision 4 -> 5c680a5f...
   Synchronizing to revision 5 -> f501a14d...
   Verifying 1 changes with the mounted tree
   hello.txt
   notes.txt
   ordinary.txt
   todo.txt
   ```

   The sync checks your local changes first, as in an ordinary working tree, then applies the new revision to the mount in place: only the paths it changes are touched, and directories it leaves alone stay as they were.

7. **Commit on a branch, then switch back to main.**

   ```bash
   lore branch create try-something
   echo 'An idea worth trying.' > idea.txt
   lore stage idea.txt
   lore commit 'Try an idea'
   lore branch switch main
   ls
   ```

   ```text
   ...
   Switching branch to main, using current remote latest revision f501a14d...
   Verifying 1 changes with the mounted tree
   Warning: Branch e7263180... is checked out in other instance(s):
     (01a0f9ac...) /home/you/my-project-b
   Switched to branch main revision f501a14d...
   hello.txt
   notes.txt
   ordinary.txt
   todo.txt
   ```

   `idea.txt` lives on `try-something` until you merge it. The warning only notes that `~/my-project-b`, from the quickstart, is on `main` too.

8. **Throw an edit away with reset.**

   ```bash
   echo 'A change to throw away.' > hello.txt
   lore status
   lore reset hello.txt
   cat hello.txt
   ```

   ```text
   ...
   Changes not staged for commit:
   M hello.txt
   Resetting file system changes
   Reset 1 (0 directories, 1 files), deleted 0 (0 directories, 0 files)
   Hello from a virtual clone
   ```

9. **Merge the branch and push the result.**

   ```bash
   lore branch merge try-something --message 'Merge try-something into main'
   ls
   lore push
   ```

   ```text
   Starting merge of branch 01a0f9ac... revision 291802d3...
   Merged files, 1 updated, 0 deleted, 0 merged, 0 conflicted
   ...
   Committed merged repository state 7 -> d91a958f...
   hello.txt
   idea.txt
   notes.txt
   ordinary.txt
   todo.txt
   ...
   Pushed revision 7 -> d91a958f... to branch main
   ```

   The merge writes its files into the mount. A merge with conflicts writes the conflicted files with markers, as in an ordinary working tree.

10. **Stop and start the service.**

    Leave the clone first: the service can't take down the mount of a clone that a shell has as its working directory.

    ```bash
    cd ~
    lore service stop
    lore service start
    ls ~/my-project-fuse
    ```

    ```text
    Lore service is running
    hello.txt
    idea.txt
    notes.txt
    ordinary.txt
    todo.txt
    ```

    Stopping the service takes down the mount of every virtual clone it serves, and starting it mounts every one it can reach again, at the revision it stood on and with your uncommitted changes in place.

## Verify

Bring the ordinary working tree up to date and compare the two trees:

```bash
cd ~/my-project
lore sync
cd ~
diff -r --exclude=.lore my-project my-project-fuse && echo 'The working trees match'
```

```text
...
The working trees match
```

Then check that the virtual clone is in sync with the server:

```bash
cd ~/my-project-fuse
lore status
```

```text
Repository 3f2a1b4c5d6e7f8a...
On branch main revision 7 -> d91a958f...
Remote revision 7 -> d91a958f...
Local branch in sync with remote
```

Both trees holding the same files, with the virtual clone at revision 7 in sync with the server, means the edit loop worked. Your hashes will differ.

## Troubleshooting

### `Lore service unavailable`, exit code 32, when you clone

```text
[Error] Lore service unavailable: A FUSE clone is mounted by the Lore service, and no service could be reached or started: starting /opt/lore/bin/lore failed: No such file or directory (os error 2)
```

No service was running, and none could be started from the executable named for it, with `lore service set-executable` or in `LORE_SERVICE_EXECUTABLE`, so nothing was created. Name one that exists, or clear the setting with `lore service set-executable ""` and unset `LORE_SERVICE_EXECUTABLE` to have the clone start the service from the `lore` you run, then clone again.

### `Repository not found`, exit code 89, in a virtual clone

```text
[Error] Repository not found: /home/you/my-project-fuse
```

No service is running, so the clone isn't mounted and its directory is empty, for example after `lore service stop` or a restart. Run `lore service start`: it mounts every virtual clone it finds again, with your uncommitted changes in place.

### A clone fails partway

A virtual clone that fails, for example on a revision that doesn't exist, leaves no mount, no directory and no instance behind. Fix the cause and run the same `lore clone` again, to the same path.

### `Local modifications prevent synchronization`, exit code 44

```text
[Error] File has local changes: hello.txt (incoming size 18 bytes, file system size 14 bytes)
[Error] Local modifications prevent synchronization
```

`lore sync` or `lore branch switch` would replace a file you changed but haven't committed, so it stops before changing anything. Commit the change, throw it away with `lore reset <path>`, or run the command again with `--force` to take the incoming version.

### `Mounts are busy`, exit code 51, when you stop the service

```text
[Error] Mounts are busy: /home/you/my-project-fuse
```

A shell or a program has its working directory or an open file inside a virtual clone. Leave the clone and close the program, then stop again. `lore service stop --force` detaches every mount at once instead, and the programs inside a detached clone lose access to it.

### `Transport endpoint is not connected`

The service that served the clone stopped without taking its mount down, for example after a crash. Run `lore service start`: it detaches the dead mount and mounts every virtual clone it can reach again. Deletions are recorded without syncing to disk, so a crash can bring back a file you deleted just before it.

## Next steps

- [Run Lore commands through the background service](run-commands-through-the-service.md) — the service that carries out every command in a virtual clone.
- [`lore clone` in the CLI command reference](../reference/lore-cli-commands.md#lore-clone) — `--vfs` and the other clone flags.
- [`lore service` in the CLI command reference](../reference/lore-cli-commands.md#lore-service) — every service command and its flags.
