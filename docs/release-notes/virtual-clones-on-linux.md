---
type: feature
---

- `lore-client`: `lore clone --vfs fuse` makes a virtual clone on Linux: the Lore service mounts the repository with
  FUSE and serves its files from the shared store for its remote, fetching them on first read, so the clone writes no
  files. A command that
  makes one, or works in one, runs through the service, which mounts its virtual clones again when it starts. With no
  service running, a command that makes one starts the service, as does a command in one whose service ended without
  taking its mount down: from the executable `lore service set-executable` names, or with none named, from the `lore`
  that ran the command. A command in a virtual clone whose service is still starting, or listens on another socket,
  starts none, and says which socket it looked on. `--vfs default` picks FUSE on Linux, SWFS on Windows and no virtual filesystem
  elsewhere, where it picked SWFS on every platform
- `lore`: `LORE_VFS_TYPE_FUSE` asks `lore_repository_clone` for a virtual clone on Linux, and
  `LORE_VFS_TYPE_DEFAULT` picks FUSE on Linux, SWFS on Windows and none elsewhere
- `lore`: `lore::serve_from_this_executable()` tells the library that the program's own executable runs the service
  with `service run`, so starting the service, and a command that makes a virtual clone, start one from it when no
  executable is named
- `lore-revision`: Virtual clones run the edit loop as an ordinary clone does, the ignore file included, apart from a
  file added with `lore file dirty` that the ignore file names: it stays marked, where an ordinary clone's scan drops
  it. A virtual clone resolves names in the case they are spelled, as an ordinary Linux clone does, so a rename
  that changes only case, such as `mv Foo foo`, is seen and staged as a move. A file or directory whose name differs
  only in case from one beside it cannot be created there, since a repository holds one spelling of a name.
  `lore status`, `lore stage` and
  `lore diff` see every write through the mount whose system call returned, with no `--scan` and no `lore file dirty`.
  A plain `lore status` lists them as it lists marked changes on an ordinary clone, a deleted directory as one entry;
  `--scan` adds the entries below it and a summary. `lore commit` stores the content the mount serves, then serves the
  committed files from the store, each keeping the file number `stat` reports and its modification time. Deletions
  through the mount are recorded without syncing to disk, so a crash may bring back the last ones. Symbolic links are
  not versioned, as on an ordinary clone: status and commit take one as nothing there, and `lore stage` naming one
  skips it, where an ordinary clone stages the file it names. Writes still meet one: a sync, merge or cherry-pick that
  would write where a symbolic link stands, or below one, stops as for a local edit
- `lore-revision`: `lore sync` and `lore branch switch` on a virtual clone check local changes first, as on an
  ordinary clone: an edit of a file the target changes stops the command with exit code 44 unless `--force` is given,
  which gives the file the target's content, and `--dry-run` stops after the check. A file deleted locally that the
  target changes is restored, unless a symbolic link stands at its path. There `--force` keeps the symbolic link: the
  target's change at that path is not applied, and status shows the tracked file deleted until `lore reset <path>`
  replaces the link with the target's file; `--forward-changes` keeps the link too, dropping the target's change at
  that path. The target is then applied to the mount in place, so directories it leaves alone keep their cached
  listings
- `lore-revision`: `lore reset` and `lore branch merge` on a virtual clone, and a `lore sync` that merges, write their
  files into the mount. A reset back to the mounted revision serves the file from the store again; other files they
  write stay local until the next commit stores them. A reset removes a symbolic link standing at a path it writes and
  puts the file in its place, never writing through the link. A conflicting merge writes the file with conflict
  markers beside its side files, as on an ordinary clone
- `lore-revision`: `lore clone --vfs fuse --prefetch <file>` warms the local store in the background from an access
  recording of up to 128 MiB after the clone returns: the recorded directories' node blocks and listings and the
  recorded files' contents. `LORE_TREE_PREFETCH_DEPTH` in the service's environment selects the tree prefetch that
  follows: `blocks` (default), a level count, `all` or `0`
- `lore-revision`: FUSE mounts record the first access to each path their revision holds, with each directory's node
  block indices, to `<dir>/<instance id>-<unix seconds>.txt` when the service's environment sets
  `LORE_FUSE_RECORD_DIR=<dir>`, as input for `--prefetch`
- `lore-revision`: `lore repository instance move` and `lore repository instance delete` work on a virtual clone.
  Moving one unmounts it and mounts it at the target at the revision it served, with its uncommitted changes; if it
  cannot be mounted there, it is mounted at the source again. The old mount point is removed once the instance is
  registered at the target, so a service that starts in between still finds it. Deleting one unmounts it before
  anything else, then deregisters it and removes its external directory and the empty mount point. Both refuse a mount
  something holds, such as a shell whose working directory is inside it, saying the mount is busy and leaving the
  instance as it was. A move is also refused while the service stops
