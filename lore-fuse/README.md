# lore-fuse

A Linux FUSE virtual file system for Lore instances, served by `lore service`. The mount serves one
filesystem `State`, the revision with the local changes applied, which every request reads and
changes in place. Tracked content is read lazily from the store; local content lives under
`write/` beside the instance.

- `lore-fuse` is the protocol and session layer. It speaks `/dev/fuse` directly, with no libfuse
  and no third-party FUSE crate (`src/lib.rs`).
- The provider, which answers requests from a revision, lives in
  `lore-revision/src/fs/fuse/`. The service holds a mount manager per VFS
  (`lore-revision/src/fs/mount_manager.rs`); virtual clones are the FUSE manager's, in
  `lore-revision/src/fs/fuse/mount_manager.rs`.

Paths: `src/…` and `tests/…` are this crate; `fs/…` is `lore-revision/src/fs/…`; `repository/…`,
`file/…`, `state/…`, `state.rs`, `commit.rs`, `immutable.rs`, `instance.rs` and `find.rs` are below
`lore-revision/src/`; any other bare file name, and `fragment_cache/…`, is below
`lore-revision/src/fs/fuse/`, so `mount_manager.rs` is the FUSE manager and `fs/mount_manager.rs`
the service's. Other paths are from the repository root.

**Requirements**

| Requirement | Where enforced |
|---|---|
| Linux. FUSE instances are refused elsewhere. | `create_local`, `lore-revision/src/repository.rs` |
| Kernel FUSE protocol 7.39 or later (Linux 6.8 ships 7.39), offering every required INIT flag | `negotiate`, `src/init.rs`; `REQUIRED_MINOR`, `src/abi/mod.rs` |
| Read-write access to `/dev/fuse` | `clone_device`, `src/transport/device.rs` |
| `fusermount3` on `PATH`. Root falls back to `mount(2)` when it is missing or fails. | `mount_with_access`, `src/mount.rs` |
| The service. A FUSE instance is only created inside the service process, and a command that needs one starts one when none is running. | `create_local`, `lore-revision/src/repository.rs` |

## Trying it

```sh
cargo build --release -p lore-client          # builds the `lore` binary
export PATH=$PWD/target/release:$PATH
export LORE_SERVICE_EXECUTABLE=$PWD/target/release/lore   # this build serves, over [service] executable
export LORE_SERVICE_SOCKET=lore-fuse-try       # a service of its own, apart from any other

# Clone into a FUSE mount. The clone runs in the service, and with none running on this socket
# starts one. --view and --prefetch are optional.
lore clone <url> ~/work/tree --vfs fuse \
    --view <view filter file> --prefetch <recording>

ls -l ~/work/tree; cat ~/work/tree/README.md    # read through the mount
echo hi > ~/work/tree/new.txt                  # lands in <global data>/external/<instance id>/write

cd ~/work/tree
lore status                                    # sees the write at once, no --scan
lore stage new.txt && lore commit "add new.txt" && lore push
lore sync                                      # checks local changes, then swaps the revision
lore branch switch <branch>                    # also a swap

lore service stop                              # refused while a mount is busy
lore service stop --force                      # detaches busy mounts lazily
lore service start                             # remounts every FUSE instance
```

- `--vfs fuse` selects `VfsType::Fuse`. `--vfs default` also means FUSE on Linux
  (`LoreVfsType::to_config`, `lore/src/repository.rs`); the flag's default is `none`.
- A FUSE clone, and every command run against an existing FUSE instance, goes to the service
  whatever `LORE_USE_SERVICE` says (`command_requires_service`, `lore/src/call_delegation.rs`).
  With no service running it starts one: from the named service executable, or with none named,
  from the `lore` client that ran it, which calls `lore::serve_from_this_executable()`
  (`executable_to_start`, `lore/src/remote/service_process.rs`). A program that links the
  library and names none fails with `ServiceUnavailable` (exit code 32) and creates nothing
  (`forced_service_call`, `lore/src/call_delegation.rs`). `lore service start` starts one the
  same way. A command in an instance whose mount answers while nothing listens on its socket
  starts none: the service serving the mount is still starting, or listens on another socket. The
  error names both causes and the socket it looked on.
- An instance is found as FUSE through its mount (`discover_external_dot_lore`), so a command at
  the path of a clone no service has mounted, after `lore service stop` or a reboot, does not see
  the instance and starts no service. `lore service start` mounts it again.
- `LORE_USE_SERVICE` relays any other command; `0`, `false`, `no` and `off` read as off
  (`reads_as_off`, `lore/src/remote/service_process.rs`).
  `lore service set-use-automatically true` makes relaying the default.
- A relayed command starts a service only from a named service executable:
  `LORE_SERVICE_EXECUTABLE`, or `lore service set-executable <path>` (`[service] executable` in the
  global config; `resolve_service_executable`, `lore/src/remote/service_process.rs`). Relaying
  to a running service needs none.
- `LORE_SERVICE_SOCKET` names the service socket; processes sharing a value share a service
  (`LORE_SERVICE_SOCKET_VAR`, `lore/src/remote.rs`). `.cargo/config.toml` sets `LORE_USE_SERVICE=0`
  and `LORE_SERVICE_SOCKET=lore_service-cargo` for anything `cargo` runs. Override both when trying
  the mount through `cargo run`.
- A FUSE clone always uses the shared store for its remote, as if `--use-shared-store` were given,
  and refuses `LORE_SHARED_STORE_MODE_DISABLED` (`LoreSharedStoreMode::for_vfs`,
  `lore-revision/src/repository.rs`): the service remounts only instances it finds through the
  shared store registry (`mounts_of_entries`, `fs/mount_manager.rs`).
- The instance's `.lore` directory lives outside the mount, at
  `<global data>/external/<instance id>/.lore` (`external_dir_for_instance`,
  `lore-revision/src/global/external_dir.rs`). Commands inside the mount find it through
  `/proc/self/mountinfo`, never by statting the mount (`fs/fuse/discover.rs`).

**Prefetch settings and recording.** The service reads `LORE_TREE_PREFETCH_DEPTH` and
`LORE_FUSE_RECORD_DIR` from its own environment, so set them when starting it:
`LORE_TREE_PREFETCH_DEPTH=2 lore service start` warms two levels of each clone's tree instead
of every block (see "Prefetch"). To record a prefetch file:

```sh
lore service stop
LORE_FUSE_RECORD_DIR=~/recordings lore service start   # every mount this service constructs records
lore clone <url> ~/work/rec --vfs fuse --view <view filter file>
(cd ~/work/rec && <build>)                              # the workload to record
lore service stop                                       # the recording is ~/recordings/<instance id>-<unix seconds>.txt
lore service start                                      # without the variable: recording off
```

**Removing or moving a virtual clone.** `lore repository instance delete` and
`lore repository instance move <path>`, run from outside the clone, do it in the service:

```sh
cd ~ && lore repository instance delete --repository ~/work/tree
cd ~ && lore repository instance move ~/work/elsewhere --repository ~/work/tree
```

- Both refuse with `MountsBusy` (exit code 51), changing nothing, while anything holds the mount,
  such as a shell whose working directory is inside it. Moving is also refused while the service
  stops.
- Deleting a clone unmounts it first, then deregisters it and removes its external directory and
  the empty mount point (`instance_delete`, `lore-revision/src/instance.rs`).
- Moving a clone unmounts it and mounts it on a new mount point at the target, at the revision it
  served, with its uncommitted changes. The old mount point stays, empty, until the registration
  names the target, so a service that starts in between mounts the clone where it is still
  registered (`move_mount`, `mount_manager.rs`; `remove_left_mount_point`,
  `instance.rs`). If the clone cannot be mounted at the target, it is mounted at the source
  again.
- A mount point holding no checkout is an unmounted clone, not a removed one: pruning keeps its
  registration while its external directory names a virtual clone (`instance_staleness`,
  `instance.rs`).

By hand, unmount first: deleting a mounted tree deletes through the mount, building every
directory's listing and appending each tracked path to the deletion list.

```sh
lore service stop                  # --force if a mount is busy
rm -rf ~/work/tree                 # now an empty mount point
lore service start                 # removes the deleted instance's external directory, mounts the rest
lore repository instance prune     # optional, from another instance of the same repository
```

- Service start removes `<global data>/external/<instance id>/`, holding `.lore` and `write/`, of
  every registered FUSE instance whose mount point no longer exists, and does not mount it
  (`remove_deleted_instance`, `mount_manager.rs`). A path that cannot be reached is left
  alone. A running service removes nothing.
- The instance stays registered in the shared store until `lore repository instance prune` runs
  in another instance of that repository (`instance_prune`, `lore-revision/src/instance.rs`).
  The content the clone fetched stays in the shared store for other instances;
  `lore shared-store list` names the registered stores.
- A virtual clone that fails inside the service undoes its mount: the service detaches it
  lazily, removes its external directory and, where the clone created the mount point, removes
  it once empty (`abandon_clone`, `mount_manager.rs`, called from `clone`
  through `abandon_failed_fuse_clone`, `repository/clone.rs`, and from `create_local`,
  `lore-revision/src/repository.rs`). The registration goes at the next prune, and the path
  clones again.

**Stale mounts.** A service that dies leaves a mount answering `ENOTCONN`. On start the service
detects this with `statfs` and detaches it lazily before remounting (`remount_instance`,
`mount_manager.rs`; `is_stale_mount`, `src/mount.rs`). By hand: `fusermount3 -uz <path>`. Only one
service remounts: each claims its socket name (a lock beside the socket) before it mounts anything,
so a second service, such as one a racing command starts, mounts nothing; it waits for the first to
answer, then exits (`service_main`, `refuse_claimed_socket`,
`lore/src/service/service_main.rs`). Without the claim it would mount over the
first, lose the socket and leave dead endpoints.

## How it works

### Protocol and session

- **INIT.** Worker 0 answers INIT before any other worker starts (`negotiate_init`,
  `src/session.rs`).
  - Required flags: `ASYNC_READ`, `DO_READDIRPLUS`, `READDIRPLUS_AUTO`, `PARALLEL_DIROPS`,
    `MAX_PAGES`, `INIT_EXT`. A kernel missing one, or below minor 39, is rejected
    (`REQUIRED_FLAGS`, `negotiate`, `src/init.rs`).
  - Taken when offered: `EXPLICIT_INVAL_DATA`, `HAS_EXPIRE_ONLY`, `NO_EXPORT_SUPPORT`, and
    `BIG_WRITES` when `max_write` is over one page; without it the kernel sends one page per
    WRITE (`negotiate`, `src/init.rs`). `WRITEBACK_CACHE` is never requested.
  - `NO_OPEN_SUPPORT` when zero-message open is configured. The service mount configures it
    (`mount_with`, `fs/fuse/mount.rs`).
  - `max_write` is `InitConfig::max_write`: 4096 by default, 1 MiB from `InitConfig::read_write()`,
    clamped to at least one page and at most `max_pages` pages (`negotiated_max_write`,
    `src/init.rs`). `time_gran` 1, `max_readahead` the kernel's offer capped at `max_pages × 4 KiB`.
- **Workers.** One device fd per worker, cloned with `FUSE_DEV_IOC_CLONE` and polled non-blocking
  through `AsyncFd` (`DeviceTransport::worker`, `clone_device`,
  `src/transport/device.rs`). Each worker allocates a request buffer
  and a `max_pages`-page reply buffer once and reuses them (`worker_loop`, `src/session.rs`). The
  request buffer holds one full WRITE, 80 bytes plus `max_write`, and at least 16 KiB; the kernel
  reads no request into a smaller buffer (`RequestBuffer::for_max_write`, `src/buffer.rs`).
- **Opcodes.** Requests decode into views of the request buffer, without copying
  (`decode`, `src/codec/request.rs`). Besides the read path, the write family is decoded and routed:
  WRITE, CREATE, MKNOD, MKDIR, UNLINK, RMDIR, RENAME, RENAME2, SETATTR, SYMLINK, READLINK, LINK,
  FLUSH, FSYNC, FSYNCDIR, FALLOCATE. RENAME and RENAME2 reach one `rename` method; RENAME passes
  flags 0.
  - A WRITE payload is a slice of the request buffer; dispatching a WRITE allocates nothing
    (`tests/mount_read_write.rs`, `write_dispatch_allocates_nothing_after_warmup`).
  - The write-family `Filesystem` methods default to `default_errno`: `EROFS`, `ENOSYS` for
    FLUSH, FSYNC and FSYNCDIR, `EINVAL` for READLINK (`src/filesystem.rs`). The provider
    answers them from the filesystem State and `write/`, or with those defaults when it has no
    `write/` (see Writes).
  - SETXATTR, REMOVEXATTR, COPY_FILE_RANGE and TMPFILE, which no provider implements, answer
    `ENOSYS` (`default_errno`, `src/filesystem.rs`): the kernel then copies a `copy_file_range(2)`
    with splice, so `cp` works, and answers `EOPNOTSUPP` to `setxattr(2)`, `removexattr(2)` and
    `O_TMPFILE` without asking again.
  - CREATE replies with the entry and an open handle. With zero-message open, a WRITE on a
    descriptor CREATE opened carries that handle, and on any other descriptor handle 0
    (`tests/unit/mount_read_write.rs`).
  - An aborted reply to CREATE, MKNOD, MKDIR, SYMLINK or LINK reports `ReplyOutcome::Entry`, as
    LOOKUP does.
  - The write family does not race INTERRUPT once it changes anything: a modifying request runs
    to completion (see "Deferred writes").
- **Mount access.** `mount` mounts read-only: the `ro` option, and `MS_RDONLY` in the `mount(2)`
  fallback. `mount_with_access` with `MountAccess::ReadWrite` drops both
  (`MountOptions::option_string_for`, `privileged_mount_flags`, `src/mount.rs`). The service mounts
  read-write with `InitConfig::read_write()` (`FuseMount::mount_writable`, `mount_with`,
  `fs/fuse/mount.rs`; `create_mount`, `remount_instance`, `mount_manager.rs`);
  `FuseMount::mount` stays read-only for benchmarks.
- **Deferred requests.** READ, LOOKUP, READDIR and READDIRPLUS first run on the worker without a
  remote fetch: `read` with `fetch: false`, `lookup_local`, `readdir_local`, `readdirplus_local`
  (`src/filesystem.rs`). A READ misses on a fragment that is not local; the others miss
  when the directory's listing is not in the listing cache (`cached_listing`,
  `fs/fuse/filesystem.rs`). A miss returns `ReadFault::WouldFetch`, which only the local
  forms may return; the dispatcher spawns a task that owns what the answer needs, runs the
  fetching form (`read` with `fetch: true`, `lookup`, `readdir`, `readdirplus`) and replies on the
  same worker's fd, so neither a fetch nor a listing build holds a worker (`defer`,
  `src/dispatch.rs`). GETATTR reads the inode table only and never fetches.
- **Deferred writes.** CREATE, MKNOD, MKDIR, SYMLINK, UNLINK, RMDIR, RENAME, WRITE, SETATTR,
  FALLOCATE and FSYNC run their local forms on the worker (`create_local` ... `fsync_local`,
  `src/filesystem.rs`), not raced against INTERRUPT. The provider answers `WouldFetch` before any
  change where a directory has no cached listing, where a WRITE, SETATTR, FALLOCATE or RENAME
  would copy up a tracked file's content (a truncate to zero reads none and copies up on the
  worker), and for every flush of a local file. A CREATE, MKNOD, MKDIR or SYMLINK takes its node
  slot from a fresh block, which is in memory, so the slot never defers it (see "File metadata and
  new nodes" below); the attach itself reads only what is in memory
  (`ServedNodes::node_attach` with `AttachCheck::Resident`), deferring where its walk for the name
  or its served mark of the directory needs a block that is not. Where the directory's listing and chain log show the name
  absent (`Listing::lookup_to_attach`), the attach, and a RENAME's reattach onto a free name, check the
  log's patches since under their write locks instead of walking the chain for the name; a log
  that moved past that position, or a listing that left a child out, walks as before
  (`State::node_attach_absent`, `State::node_reattach_absent`). A stale cached listing does not
  defer: the State
  answers the name from the blocks the listing holds, which include those of the children it
  leaves out. RMDIR checks emptiness in the directory's chain, reading only blocks and name tables
  in memory on the worker: a warm `rm -rf` defers nothing, while an RMDIR of a directory whose
  chain is not in memory, as one never listed, defers and loads the chain off the worker. The
  deferred task runs `Filesystem::fetch` raced against INTERRUPT (the listings, the copy-up
  content, or the flush through `lore-io`), then the full form as the completion, not raced: an
  INTERRUPT during the completion gets its result. The copy-up content is staged under
  `write-staging/`, only the bytes a truncate keeps, by a task that owns the staged file, so an
  interrupted fetch removes it only once every host write into it has finished. The completion
  resolves everything again, so a directory changed or a listing evicted meanwhile is read afresh,
  and a staged copy is used only while it is still the file's content; a RENAME drops its source's
  staged copy whatever its outcome (`fs/fuse/write.rs`, `stage_copy`, `copy_up_held`,
  `rename_node`).
- **Interrupts.** LOOKUP, GETATTR, READDIR, READDIRPLUS and the local half of READ race against a
  per-worker in-flight slot and reply `EINTR` when an INTERRUPT wins (`src/interrupt.rs`). Deferred
  requests are found through a `DashMap` of uniques. An INTERRUPT that finds no target gets
  `EAGAIN`, so the kernel requeues it (`src/dispatch.rs`, the `Request::Interrupt` arm of
  `dispatch_request`).
- **Reply accounting.** A reply the kernel aborted (`ENOENT` on write) calls `reply_failed`, which
  rolls back the lookups that LOOKUP or READDIRPLUS counted (`fs/fuse/filesystem.rs`,
  `reply_failed`). A READDIRPLUS page counts one lookup per record it writes with a nonzero node
  id, `.` and `..` aside, as the kernel takes them. The dispatcher reads those node ids back from
  the page itself when the kernel aborts the reply, when the page answers an error, and when an
  INTERRUPT cuts the page short while a later row waits on the store, so a RENAME since the page
  was built cannot send the rollback to another inode (`src/dispatch.rs`, `give_back_records`;
  lore-lti).
- **Zero-message open.** The provider answers OPEN with `ENOSYS`. The kernel then stops sending
  OPEN for the connection and keeps its page cache (`open`, `fs/fuse/filesystem.rs`).
- **Notifications.** `inval_inode` and `inval_entry` go out on their own cloned fd with a raw
  `writev`, never under a lock; `ENOENT` is success (`src/notify.rs`). The write blocks in the
  kernel whatever `O_NONBLOCK` says: an entry invalidation takes its directory's lock, which a
  LOOKUP there holds until the daemon answers it, and an inode invalidation waits on an in-flight
  READ's page locks. So none is sent from a runtime worker, whose tasks may include that answer,
  nor from the core blocking pool, whose 2 threads carry the service's connections (lore-atf).
  - **Each mount has a notification thread** (`fs/fuse/notifications.rs`, 128 KiB stack),
    started when the session hands over its notifier; a mount whose thread does not start fails.
    Swaps and instance operations queue a batch, entries before inodes, and await it; batches go
    out in the order queued, even when the sender is dropped while it waits. Unmount stops the
    thread once the session has stopped and waits for it to end (`FuseMount::unmount`).
  - An instance operation (`write_file`, `rename`, `remove`, ...) collects its invalidations and
    sends them as one batch before it returns, each directory inode once. Dropped before then,
    it queues what it collected without waiting (`Pending`); so does a swap dropped once it has
    planned.
  - A request answering a change queues the stale spellings it leaves without waiting
    (`invalidate_spellings`).
- **Metrics.** Per-opcode log2 latency histograms and allocation high-water marks, lock-free and
  allocation-free (`src/metrics.rs`). Allocations are counted while `set_allocation_counting(true)`
  is in effect. The count covers the global allocator and the private rpmalloc heaps (node blocks,
  GrowVec chunks).

### Mount lifecycle in the service

| Event | Behaviour | Code |
|---|---|---|
| Clone | Mounts an empty `State`, then the clone's swap publishes the revision. Records `vfs.base_time` in the instance config. The hold fetches nothing beyond the revision's state and tree headers, and starts a background prefetch that warms the store (see "Prefetch"); the provider loads the rest on demand (`clone_materialize_hold`, `repository/clone.rs`). While the state's records cache, the clone reads the root's ignore file into the store, so a command run offline finds it, and returns once both are done (`clone_onto_held_state`, `repository/clone.rs`). | `create_mount`, `mount_manager.rs`; `create_local`, `lore-revision/src/repository.rs` |
| Failed clone | Detached lazily, its external directory removed, and the mount point removed where the clone created it and it is empty; the registration goes at the next prune. | `abandon_clone`, `mount_manager.rs`; `clone`, `abandon_failed_fuse_clone`, `repository/clone.rs` |
| Service start | Mounts each registered FUSE instance at its current revision. A store it cannot open or list, and an instance whose `.lore` or config it cannot read, is logged and skipped, and the rest are mounted. An instance whose mount point was deleted has its external directory removed and is not mounted; a path that cannot be reached is left alone, and the next prune of the store retires the registration. | `mounts_of_entries`, `fs/mount_manager.rs`; `remove_deleted_instance`, `remount_instances`, `mount_manager.rs` |
| Stale endpoint | Detected by `statfs` → `ENOTCONN`, lazily unmounted before remount. | `remount_instance`, `mount_manager.rs` |
| Delete | `lore repository instance delete`: unmounts the clone, which a busy mount refuses with `MountsBusy`, then removes its registration, its external directory and the empty mount point. | `instance_delete`, `lore-revision/src/instance.rs` |
| Move | `lore repository instance move`: refused while the service stops; unmounts the clone, which a busy mount refuses with `MountsBusy`; mounts it on a mount point made at the target at the revision it served; registers it there; removes the old mount point. When mounting or registering at the target fails, it mounts the clone at its source again. A crash after the mount at the target and before the registration leaves a stale endpoint there, which `fusermount3 -uz <target>` detaches; the next start mounts the clone at its source. | `move_mount`, `mount_manager.rs`; `instance_move_with`, `instance.rs` |
| Stop | Plain unmount of every FUSE mount. If any is busy, the ones already unmounted are remounted at the revision they served, and the stop fails with `MountsBusy` (exit code 51) naming the paths. A mount someone else already detached counts as unmounted. From its snapshot of the mounts until it is refused, a stop makes a mount another command creates detach and fail. | `unmount_all`, `refuse_while_stopping`, `mount_manager.rs` |
| Stop with the global `--force` | Every mount is detached lazily (`fusermount3 -u -z`, or `umount2(MNT_DETACH)` as root) and forgotten; nothing is remounted, and a path that stays attached is named in a warning while the stop goes ahead. Mounts a refused stop remounted are back in the table, so a later forced stop detaches them too. | `unmount_all`, `mount_manager.rs`; `unmount_path`, `src/mount.rs` |

- Unmount retries a busy `fusermount3 -u` 5 times, 20 ms apart, since desktop indexers briefly
  hold new mounts (`unmount_path`, `src/mount.rs`).
- Busy cannot be asked in advance: `umount2(MNT_EXPIRE)` needs `CAP_SYS_ADMIN`, and a `/proc`
  scan cannot see other users' processes (`unmount_all`, `mount_manager.rs`).
- An unmount, plain or lazy, detaches the mount, stops the session and cancels the background
  prefetch (`FuseMount::unmount`, `fs/fuse/mount.rs`). Nothing is left to record: every
  request changed the State and `write/` before it was answered.
- A provider claims its instance's stores when it is built and holds them for as long as it
  serves them, as a command does for its duration
  ([ADR-00024](../docs/developing/decisions/00024-store-locks-held-per-span-contain-the-repository-lock.md)):
  another process using the same stores waits until the mount is gone, and the kernel's requests
  take no lock of their own, nor reload the stores between them. Every context the provider serves through shares
  the claims, so a readahead, prefetch or deferred request still running after the unmount keeps
  them until it ends (`claim_provider_stores`, `provider_context`, `filesystem.rs`).
- A provider reaches the remote through a connection of its own, which every context it serves
  through shares: connected from the instance's configuration whatever the command that built
  it was told about going offline, since content it serves later may be stored on the remote
  alone (`RepositoryContext::serving_remote`). A clone or create makes it in the caller's
  execution context, so it authenticates with the caller's credentials, as the external
  directory is resolved with the caller's global path; a remount after a move keeps it, and a
  service start makes it in the service's context (`create_mount`,
  `remount_instance`, `mount_manager.rs`). The connect starts when the mount is made, so
  the mount has exchanged its token for the partition before the first read.
- A mount a relayed call makes is built under the service's context with that call's
  environment (`LORE_GLOBAL_PATH`, `LORE_AUTH_PATH`), which it keeps for its life: a request it
  serves after the command ended reads the token store and configuration the caller reads, as
  a reconnect or a new storage session does (`in_serving_context`,
  `ExecutionContext::serving`). A mount restored at service start has no caller and reads the
  service's own.
- With debug assertions, a plain unmount first invalidates the root's children, again every
  50 ms while the inode table counts lookups, until it has counted none for 100 ms or 2 s have
  passed. After it, the unmount warns about inodes still holding lookups
  (`drain_kernel_lookups`, `check_leaks`, `fs/fuse/mount.rs`).

### The served State and swaps

- Every request reads what is served: the one filesystem `State`, the `RepositoryContext` (and so
  the view filter) and the listing cache (`fs/fuse/served.rs`). It sits in a
  `parking_lot::RwLock<Arc<Served>>`; a load is a read lock and a refcount increment.
- Clone, sync and branch switch hand the provider the target revision alone
  (`InstanceOperationImpl::hold_state`, `fs/filesystem_provider.rs`; `realize_state`,
  `fs/realize.rs`), with `HoldOptions`: `--force` overwrites local changes, forwarding local
  changes keeps them, and `--dry-run` changes nothing. The provider takes it in
  `swap::hold_mounted`, through the provider-agnostic `fs/hold.rs`: it diffs the served revision,
  read clean, against the target (`hold::changes`), checks the local changes the served State
  carries against that diff (`hold::check`), applies it (`hold::apply`), and reports it as a sync does: the totals once
  discovered, a sync event for each change the view shows, then the totals with the work done
  (`hold::report`); the mount's own side (times, `write/`, the
  kernel's caches, publishing) stays in `fs/fuse/swap.rs`. Nothing realize diffs or verifies is
  used.
  After the hold, a sync or switch reads the target's ignore file into the store
  (`InstanceOperationImpl::warm_held_ignore_file`, `fs/filesystem_provider.rs`, called from
  `realize_state`, `fs/realize.rs`); the Clone row above gives a clone's order.
- A view change also checks and reports what enters and leaves the view: the diff of the served
  revision under the served view against the target under the target's view
  (`hold::shown_changes`), so a narrowing past a local change refuses unless forced or
  forwarding, and the sync reports each path leaving the view as deleted and each path entering
  it as added.
- The check reads the State's flags, so it costs nothing where no child of the root is dirty,
  and reads content only for a local edit of a tracked file at a path a change replaces or
  removes, or below a directory it removes: one holding what the change replaces or what it
  brings, or below the directory what the revision holds at its path, as a file renamed only in
  case does, loses nothing, so it passes, and the apply gives it the target's content, or removes
  it, as a forced sync does (`hold::check`, `removal_loses`, `Matching`). Otherwise a local file
  or link at a path the target changes, a local edit at a path it removes or moves away, a
  directory it removes holding any other local edit, a directory it turns into a
  file holding any local change, or a change below a local file or link standing where a directory
  was, fails the sync with `LocalModifications` (exit 44), unless forced or forwarding; a file
  added locally in a directory the target removes refuses nothing. Where a local replacement
  stands beside the node it displaced under one folded name, the local one is checked. A file
  added locally, and a link, count as changed whatever they hold. It logs
  `Verifying N changes with the held state`, as an ordinary clone logs the changes it verifies
  against its files, and names each path it refuses. A dry run stops after the check and reports
  what the sync would change. An edit landing between the
  check and the apply is kept, as the apply overwrites no local file unless forced or matching,
  and the sync succeeds; one landing on a matching file is overwritten, as an ordinary sync's
  write lands over a file it compared. A forced sync overwrites a local edit of a file the target changes or removes,
  keeping files added locally and the directories above them, and a sync that keeps no local
  change restores a file deleted locally that the target changes, re-attaching the directories
  above it that were deleted too. Host paths follow the State's spelling of a name. Swaps are
  serialized, with each other and with a commit's reconciliation (`swap_plan`).
  The first publish onto the empty revision a mount starts on only publishes and invalidates the
  root and the names the kernel looked up; it diffs and records nothing, so a clone costs the
  same for any tree size (`fs/fuse/swap.rs`, `is_unpublished_empty`). Any other swap:
  1. takes what the State applies: the unfiltered diff from the served revision, plus a
     re-placement of each path an earlier failed or cancelled apply changed, from what the served
     State holds there to what the target holds, so no failed target is ever loaded and a local
     deletion is never undone, and checks it;
  2. records the swap time for written files and for directories whose entry set changed, and
     stores those times before anything is published;
  3. applies it to the served State in place, never losing a local change and placing nothing
     below a directory it copied whole, with the swap count odd while it runs: an inode the
     kernel holds takes no content then, and a new or revived inode read before the count moved
     is refused. Every cached listing stays, and a listing built meanwhile is cached: lookups
     replay the apply's attaches and detaches from each directory's chain log. A view change
     publishes a fresh listing cache, sharing the old one's pinned-block charge, so no listing
     built under the old view is served under the new one. Each path the apply changes is
     recorded; a copy's path is recorded before the copy as one to add back, and marked settled
     once the copy lands. An apply that fails or is cancelled republishes the served
     repository, orphans the file inodes at those paths, and leaves them for the next swap,
     which re-places each one and makes a directory there match its target whole;
  4. once the apply completes, gives each directory above a change the target's address, clean,
     where its children are the target's, and marks it dirty again where a child turned dirty
     meanwhile (`settle_swapped`); rewrites the deletion list against the target, and removes
     the host directories of tracked directories it detached (`remove_host_tree`). An
     overwritten local file leaves `write/` under a settle claim, so no inode is made from a read
     before it, and its inode turns store-served under its write guard
     (`overwrite_local_file`); a local inode the plan finds there later was made local again
     by a write since, which wins (`plan_overwritten`);
  5. plans invalidations from the inode table and the spelling record, from what was applied
     where anything was unsettled: a file whose change differs in content is invalidated whatever
     its inode holds. It orphans replaced inodes, stamps touched directories, and gives
     rewritten inodes the target's content, keeping no change past the swap, since a change's
     mapping holds the command's context and with it the write token;
  6. sends `inval_entry` and `inval_inode` with no table lock held, from the mount's notification
     thread, and waits for them under the swap's lock (`send`, `fs/fuse/swap.rs`).
- A request answered while a swap superseded what it read replies with zero entry timeouts
  (`timeouts_for`, `filesystem.rs`).
- A directory stream pins the listing of its first READDIR, so offsets stay valid across a swap
  (`DirHandle`, `filesystem.rs`).

### State node operations

- Every change the provider makes to the served State's nodes, their chains and their flags goes
  through the node operations in `lore-revision/src/state/node/`. A node block's write is visible
  only inside the State's module, so a writer outside it does not compile.
- The provider's requests race its own settles and swaps, so the module keeps the tree whole under a
  written contract, in its module documentation. Requests use the served tier (`State::served`):
  every chain change and every dirtying needs a dirty parent, not flagged `Discarded`, with no clear
  pending, so a settle or a swap can trust a directory's dirty flag at every moment. A swap's apply
  and settle use the swap tier (`State::swap`), which publishes clean nodes and detaches only clean
  nodes, empty directories and local files at their recorded place.
- Each checked create, rename and move decides under its block locks, against a walk checked by
  the chain's change count or a listing's log, so two requests racing for one name never both
  land. A request dropped or refused at any point leaves the State whole, at worst leaking a slot
  or leaving a directory dirty. The provider does not yet sweep the slots left that way
  (Remaining work, "Detached slots of the served State").
- The host steps around an operation, the moves under `write/`, the deletion list and the settle
  log, are not held to that contract yet (Remaining work, "Host steps around State operations").
- The deterministic scenarios in `lore-revision/tests/state_scenarios`, the provider's commit
  settle among them (`lore-revision/tests/state_scenarios/fuse.rs`), and the stress suite in `scripts/state-stress` run the operations
  concurrently and check every outcome against a model
  of the contract.

### Writes

A writable provider (`FuseFilesystem::new_writable`, `filesystem.rs`) serves one filesystem
`State`, mutated in place inside each request, and keeps the bytes of local content under the
instance's external directory (write family in `fs/fuse/write.rs`). The service passes that
directory (`external_dir_for_instance`, `lore-revision/src/global/external_dir.rs`), which holds
`write/`, the staging directory and the deletion list. Nothing else is persisted: the
State is rebuilt at every mount.

| Path below `<global data>/external/<instance id>/` | Holds |
|---|---|
| `write/` | Created and copied-up files, directories and symlinks at their repository-relative paths, spelled as the State spells them. The host file system owns their bytes, sizes, modes and times. |
| `write-staging/` | Copy-ups and stored content in progress; emptied at mount. |
| `deleted` | The deletion list (`fs/fuse/deletions.rs`): one repository-relative path per line, appended through one descriptor kept open with `O_APPEND` and never synced. The filesystem State built at mount detaches each listed path the revision holds, except one `write/` recreates with the same kind. |

- **Rebuild.** A mount builds the filesystem State from the revision, the deletion list and
  `write/` alone (`FuseFilesystem::new_writable`, `filesystem.rs`; `fs/fuse/local_tree.rs`,
  `fs/fuse/deletions.rs`): each listed path the revision holds is detached, then every entry under
  `write/` becomes a dirty `NodeExtFlags::LocalContent` node, so `write/` wins over a deletion of
  the same path.
- **Deletion list and crashes.** UNLINK and RMDIR of a path the revision holds append one line, with
  no `fsync` (plan decision 2). A crash may lose the last appends, and those paths reappear at the
  next mount. A commit, a sync and a switch rewrite the list from what the State no longer serves
  (`rewrite_deletions`, `write.rs`; `DeletionList::rewrite`, `deletions.rs`): the new list is
  written beside the old one, synced and renamed over it, and an append racing the rewrite lands in
  the list that stays. The rewrite carries whole lines, from the start of the line its length ends
  inside, so a length read mid-append (as on tmpfs, which grows a file page by page) can't turn a
  fragment into a path (`carry_lines`, `deletions.rs`).
- **Names.** Every request resolves names in the State, through the directory's cached listing
  and the nodes attached to it since (`resolve`, `write.rs`; `lookup_hit`,
  `filesystem.rs`). There is no overlay: a node is tracked, or local where it carries
  `LocalContent` (`entry_flags::LOCAL`, `listing.rs`), and a deleted name is simply absent.
  CREATE, MKNOD, MKDIR and SYMLINK attach a dirty, zero-address `LocalContent` node
  (`attach_local`, `write.rs`); UNLINK and RMDIR detach the node and record the path when the
  revision holds it (`detach_node`, `record_deletion`, `write.rs`); RENAME reattaches
  the node, replacing a held name in the same critical section (`rename_in_state`,
  `write.rs`, called from `rename_onto`), then moves its inode over the one the kernel held at the
  name, also in one critical section, so a LOOKUP never finds the name without a live inode
  (`InodeTable::move_child`, `inode.rs`). A pass over a copy of the name's index entry that
  finds none reads the entry again, and answers absent only once it is unchanged
  (`first_in_entry`, `inode.rs`). A RENAME holds the name's `arriving` claim from before it
  moves the node until its inode stands at the name, and CREATE, MKNOD, MKDIR and SYMLINK hold it
  until theirs is made. A LOOKUP that finds the name missing, or held by an inode that is not the
  node's, while the claim is held waits for one release of a claim on the name and looks again,
  rather than making a second inode for the node; on a worker it answers `WouldFetch`, so the wait
  runs deferred, and READDIRPLUS lists the row plain (`child_matching`, `filesystem.rs`). A
  new inode is admitted only while the node's record, read again under the inode table's shard
  lock, still holds the name (`insert_child`). A LOOKUP that read the replaced holder's record
  before takes the inode it finds at the name as it stands, once the record, read again, no
  longer holds the name (`identity_refresh`). The State primitives are attach, detach and
  reattach ("link" names linked repositories): each runs under the parent's block write lock,
  and a reader holding a detached node walks on through its kept sibling (plan decision 3).
  Requests call them through the served tier (`State::served`, `lore-revision/src/state/node/`),
  under the served chain rule: a change, and the copy-up's rewrite, goes ahead only under a
  parent that is live and dirty with no settle's clear pending, checked under the parent's block
  lock. A parent failing the rule is first marked with the served mark, which climbs to the first
  ancestor admitting a change and marks each level below it top-down, one pair of block locks at
  a time. So no request leaves a clean directory above a dirty node, at any step or on a
  cancel, and a swap's clean detach (`State::swap`, whose own changes need only a live parent)
  never removes local content below a request's change. No request marks a directory after its
  change. A settle, a commit's or a swap's, clears a directory through the served clear
  (`ServedNodes::node_clear`, `state/node/clear.rs`), in three phases. Under the directory's block
  lock it refuses a directory no longer at the place the settle's path resolution found
  (`Moved`: the settle looks the path up again by name and settles whatever directory stands
  there, at most `CHAIN_WALK_RESTARTS` times), drops its add or move action whatever the outcome,
  marks its clear pending and reads its chain count. It then compares the children with the
  committed (or target) directory's by name hash, kind, content hash and mode, as a multiset,
  refusing at a dirty or local child. Last, under the lock again, the directory takes the
  committed address, clean, only where the comparison held, the pending clear survived, the
  count is unchanged and its name is the committed spelling byte for byte. A change below
  meanwhile refuses and marks the directory again, which drops the pending clear; a dropped
  settle drops it too. Every served move of a directory, a respell included, dirties it and drops
  its pending clear under the same lock, so a settle either reads it where it went or keeps it
  dirty; a respell of a directory a settle cleared before leaves it reported under its new
  spelling.
- **Local files and the descriptor cache.** All descriptors of an inode share one host `File`,
  opened read-write at CREATE or on first use and cached per inode, since with zero-message open
  WRITE and READ carry handle 0 (`LocalFiles`, `descriptor`, `write.rs`). READ,
  GETATTR and LOOKUP of a local file answer from it: READ `pread`s it, and size, mode and times
  come from `fstat` (`read_local`, `local_attr`, `write.rs`; `host_attr`,
  `attr.rs`), so a WRITE takes no block write lock. The cache serves only the inode's own host
  file. A descriptor is opened by path only with the inode's incarnation and host epoch read
  first under its entry guard, and cached only where both still hold and the opened file's inode
  number is the entry's `host_file` (`open_descriptor`, `HostMatch`, `write.rs`).
  CREATE and copy-up set `host_file` from the file they made; a file nothing identified yet is
  identified by a second stat of the same path (`identify_host_file`, `write.rs`). Every
  transition to store-served (a commit's flip, a settle, a forced overwrite) bumps `host_epoch`
  and clears `host_file` under the entry's write guard, so a file opened by its path before can
  never pass for the descriptor; the open is retried, at most 8 times (`DESCRIPTOR_ATTEMPTS`).
  A host file the service cannot open for writing is opened read-only, and a SETATTR that adds the
  owner write bit drops that descriptor so the next use reopens it read-write (`open_existing`,
  `drop_read_only`, `write.rs`). Past `CACHED_FILES` idle descriptors close, down to
  three quarters of the budget (`evict`, `write.rs`); FORGET closes the inode's
  (`take_forgotten`, `write.rs`). READ and WRITE of an open local file allocate nothing
  (`read_and_write_of_an_open_local_file_allocate_zero`,
  `lore-revision/tests/fuse_allocations.rs`).
- **WRITE.** A WRITE to a local file brackets its host write with the inode's write generation,
  one add before and one after, under the inode's read guard, having checked there that the
  inode is still local and its descriptor the cached one (`host_write`, `write.rs`); otherwise
  it starts again, at most `HOST_WRITE_ATTEMPTS` times (`write_local_file`, `write.rs`).
  SETATTR and FALLOCATE bracket the same way. A commit's flip compares the generation, so it either
  sees the write and leaves the file dirty, or lands first and sends the write through copy-up.
- **Copy-up.** The first WRITE, SETATTR of size or mode, FALLOCATE or RENAME of a tracked file
  decodes it with `immutable::read_into_file` into `write-staging/`, moves it to its host path,
  marks the node dirty with the zero address and `LocalContent`, and turns the inode local, keeping
  its number (`copy_up_held`, `flip_to_local`, `write.rs`). A truncating SETATTR
  copies only what the new size keeps, so `O_TRUNC` decodes nothing. Remote content is staged
  ahead by a task that owns the staged file (`stage_copy`, `write.rs`; "Deferred writes").
- **Status and diff.** Nothing records the writes in the staged State as they happen, and there is
  no journal. Lore asks the provider: `FuseOperation::changes_from_filesystem_to_state` diffs the
  filesystem State against the tree lore names by address (`diff_states_into`,
  `lore-revision/src/state/diff.rs`), descending only where either side is dirty, so the walk is
  as large as the changes. Asked to mark (`FilesystemDiffIntent::MarkDirty`), the diff marks the
  tree as a `status --scan` walk marks it: `DirtyAdd`, `DirtyModify` or `DirtyDelete` with no
  extension flags, a mark the State holds no change for cleared, and an add the State no longer
  holds discarded: held on the scans' queue where one is handed in, and discarded once every scan
  has drained (`status::scan_paths`, `repository/status.rs`). The provider reports
  `holds_state` (`fs/fuse/mount.rs`), so `lore status` scans without `--scan`. Unasked, the
  scan reports what a plain status reports on an ordinary clone: a deleted directory as one
  entry, without the entries below it, and no summary line; `--scan` adds both
  (`TreeDiffPlan::for_status`, `ScanReport`, `repository/status.rs`). Entries stream in the
  order the scan finds them: the text output sorts them, and `--json` keeps that order. `lore stage`
  marks its targets through the same scan, reporting nothing, before resolving them, so
  `lore stage .` right after a write stages it (`mark_through_provider_diff`,
  `file/stage.rs`). Neither needs `--scan` or `lore file dirty`. `lore diff` reads the tree directly. The diff skips a symlink in the
  filesystem State, below a directory added as a whole too, and so does the operation's directory
  listing (`FuseDirectoryListing`), as an ordinary clone's walk does, so a symlink is never a
  change (`versioned_on_a_working_tree`). A path naming one holds nothing for the operation's
  reads (`FuseOperation::located`); sync and merge verification and stage ask whether one stands
  there (`InstanceOperationImpl::unversioned_symlink_at_or_above` and `unversioned_symlink_at`,
  `fs/filesystem_provider.rs`).
- **Status and stage cost over written files.** A written file has the zero address until a commit
  stores it, so the diff compares it by size, from its host file's `stat`, then by content through
  the operation (`measure`, `file_holds_content`, `fs/fuse/operation.rs`). Each status
  therefore reads every written file whose size equals the tree's, until a commit flips it; a file
  of another size costs a `stat`. A clean subtree costs nothing: it is skipped by address equality.
- **Commit.** As it stores each file it read, a commit tells the operation it read its files
  through the address, size and mode it stored (`InstanceOperation::stored`), on a dry run too.
  Once the revision is stored, made current and flushed, before its staged anchor is stored, and
  never on a dry run, it hands the stored revision to that operation (`hand_over_committed`,
  `InstanceOperation::committed`, `commit.rs`). A failed hand-over is logged as
  a warning and the commit stands. A commit of a link alone, of a layer alone, or carrying a staged layer is refused
  (`hold::refused`): it would make a revision current that the mount is not handed.
  `content_source` records each file it serves with its node, the directory node and name hash it stood at, and the inode the
  kernel held with that inode's incarnation and write generation, read before the file is
  resolved (`ServedContent`, `fs/fuse/operation.rs`). The FUSE hook (`FuseFilesystem::committed`,
  `fs/fuse/write.rs`) flips each recorded file nothing changed since its read, finding it by what
  was recorded and walking no path. Under the inode's write guard, the inode must still name the
  path, be the incarnation read with its generation unmoved, and `write/<path>` must be its host
  file; a number a FORGET freed and a lookup took again is a new incarnation
  (`InodeEntry::incarnation`), so a FORGET refuses only its own file. The node then takes the
  stored address, size and mode, clean, checked in place under its block lock
  (`State::node_change_in_place`); the inode turns store-served, keeping its number and
  modification time; and the host file leaves `write/`, its descriptor staying cached until
  FORGET or a later copy-up. WRITE, SETATTR and FALLOCATE bracket their host operation with the
  generation under the inode's read guard, and RENAME and UNLINK move a local file's host file
  under it. A flip therefore either sees their change and leaves the file dirty, or lands first
  and sends them through copy-up. A file no inode held at its read flips where none holds it now
  or the one now holds no write, and no written inode standing for its node was forgotten since
  the read, whatever its number or age: each inode records the node it stands for
  (`InodeEntry::node`), and a FORGET of a written one stamps the node's slot from the count
  incarnations draw from (`InodeTable::written_forgotten_since`). The inode now at the name is
  flipped only where the record, read again under its write guard, still shows no such forget:
  the forget stamps the slot under the number's shard lock, which that inode's insert took after
  it (`since` in `flip_held`, `write.rs`). A CREATE between attaching its node and making its inode, and a
  RENAME between moving its node and its inode, claim the name first, and a file at a claimed name
  is left alone (`LocalFiles::arriving`). A file no inode holds is settled after the others, its
  node never clean while its host file may go back: with its name claimed
  (`LocalFiles::settling`), its host file is taken aside with its directory held open; the checks
  run, and an inode the kernel holds at the name, found below the directory inode held before,
  must be local with no write and is held under its write guard; the node then changes and the
  inode turns store-served; and the host file is removed. A settle refuses only lookups of its
  own name: a LOOKUP waits while the name is claimed before it makes an inode, and a new inode of
  a local file whose name was settled since the request read it is rolled back and the lookup
  starts again (`FuseFilesystem::settle_touched`, checked before and after the inode is indexed;
  READDIRPLUS lists such an entry plain). Lookups of other names are never refused by a settle,
  and real swaps keep the global swap count. A file that fails a check goes back into its
  directory, wherever that has moved, with its node never changed. Settles run one at a time,
  yielding every 64. LOOKUP, GETATTR, READ, WRITE, UNLINK and RENAME that find a local file's host
  file absent wait, for a notification, while their own name is claimed, and start again where it
  was settled since they began (`FuseFilesystem::settled_at`, the names in a ring of the latest
  1,024 settles, `SettleLog`); a request on any other name neither waits nor starts again. The
  claim's drop wakes the waiters, even when a settle panics. After the flips, a committed file
  left dirty stops counting as added. The settle then visits every directory above a committed
  file, every directory the commit added or moved (its walk hands them over,
  `InstanceOperationImpl::hand_directory`), empty ones included, and their ancestors, deepest
  first, each through the served clear: a directory holding the committed children takes the
  committed address, clean, a local one turning tracked; any other stops counting as added. A
  visited path the revision holds where the State holds no directory is recorded deleted. The
  cleared directories' empty host directories leave `write/` only after the deletion list is
  rewritten, each checked first in O(depth) (its node still at the path, clean and tracked, the
  live inode there standing for it, no create claiming the name), so a commit dropped in between
  leaves each listed path its `write/` entry. One a create made again meanwhile is made again
  with the default mode. The removal is the settle's main cost for empty directories, about 9 µs
  each on ZFS; kept instead, every later mount would walk them with the revision's directories. From an operation's first served content, as a
  commit's read, until the operation ends, and from before the revision may become current until
  the settle completes with no error (`SettleOwed`), a RENAME, UNLINK or RMDIR of an added node
  records its old path as deleted too, since a commit may store it
  (`FuseFilesystem::commit_reading`, sampled before the State step and read again after; a
  RENAME reads whether the node counted as added under the move's own locks). An added node
  moved over a tracked one takes its path over and stops counting as added, as a remount would
  leave it, so its later removal is recorded. Armed,
  the settle owed records at once each added or moved directory already gone. A commit dropped or
  failed after its revision may have become current leaves it owed for the rest of the mount, so
  removals keep recording, an unsynced line each; only a failed store of the branch's latest,
  where nothing became current, releases it. A path the revision does not hold is dropped from
  the list by the next commit and skipped at mount. The deletion list keeps the paths the
  revision holds and the State does not serve from it. Directories and deleted paths are found
  through one index per State (`ChildIndex`), each directory found among its parent's children
  and each walked once, and the State stands at the revision. The commit hands over the State it
  stored, once the revision is current and before it reuses that State as the staged anchor, so
  nothing loads the revision again (`finalize_commit`, `commit.rs`); its serialize's fresh blocks
  stay held by the operation until the settle returns, so the settle reads them from memory.
- **Attributes.** Local entries report their host file's `stat` (`host_attr`, `attr.rs`).
  Tracked entries of a writable provider carry the owner write bit, which `default_permissions`
  checks (`entry_attr`, `filesystem.rs`). A directory whose entries change takes the current
  time (`touch_dir`, `write.rs`); a time set on a tracked directory saturates rather than
  overflow, and a tracked directory refuses a mode change with `EPERM`.
- **Opcodes.** CREATE, MKNOD (regular files), MKDIR, SYMLINK, READLINK, WRITE, SETATTR (size, mode,
  times; owner and group only unchanged), UNLINK, RMDIR (`ENOTEMPTY` where the State's chain holds a
  live child; the directory goes only while its chain holds what the emptiness check read
  (`Detach::IfUnchanged`), so an entry a create attached meanwhile keeps it, and RMDIR checks again,
  `EAGAIN` after 65 checks, or at once where the inode's directory is no longer the one at the
  name), RENAME and RENAME2 (`NOREPLACE`; `EXCHANGE` and `WHITEOUT` are `EINVAL`), FSYNC
  (through `lore-io` off the worker, `fsync_node`, `write.rs`; `ENOSYS` without `write/`, which
  the kernel takes as success), FALLOCATE. LINK is `EPERM`. FLUSH keeps `ENOSYS`: nothing is
  buffered, and one `ENOSYS` spares every later `close(2)` a round trip. Renaming a tracked
  directory, or a local one whose subtree is not all local, is `EXDEV` (`rename_resolving`,
  `rename_in_state`, `write.rs`); `mv` copies instead. A local directory a commit makes the
  revision's after the rename resolved it is refused too, under the State move's locks
  (`ServedNodes::node_reattach_local`), and its host directory moved back: its `write/` entry no
  longer carries what the commit flipped. A settle records a cleared directory in the settle log
  before removing its host directory, so a rename whose host move then finds none resolves again and
  answers `EXDEV`. A change of case alone respells it in place. A directory rename moves its
  host directory before it respells the inode, and a CREATE, MKNOD, MKDIR, SYMLINK, copy-up or the
  target of a RENAME builds its host path from the inode names, so one racing the rename can make
  the old name again under `write/`. Each directory entry counts the renames of it under way, from
  before the host move until the rename has respelled the inode and moved what stands under the old
  host path into the new one, and a change counter moved at each one's start and end
  (`InodeEntry::renames`, `rename_changes`, `gather_after_rename`); renames of one directory can
  overlap, as an instance rename takes no kernel lock. A create reads both above it as it builds its
  path and again once its entry exists; where a rename was under way or a counter moved, it builds
  the path again after an `ENOENT`, and moves an entry left under an old name once no rename above
  is under way, waiting at most `RENAME_WAITS` yields, then removes the old directories its own
  making created, where left empty (`at_host_path_watched`, `settle_host_entry`). Nothing on the
  warm path writes shared state.
- **Stale spellings.** A change invalidates the kernel's other cached spellings of the name from a
  blocking task; the kernel holds the parent locked until the request is answered
  (`invalidate_spellings`, `write.rs`).
- **`InstanceOperation` reads.** Every read method answers from the State and the host files,
  resolving a path through the cached listings (`located`, `fs/fuse/operation.rs`):
  `file_info`, `untracked_file_info`, `read_directory`, `holds_name_exactly` and
  `names_folding_to` from the listing entry, or a local entry's host file;
  `content_source`, `file_holds_content` and `infer_is_diffable` from the host file of a local
  file and the store for a tracked one (`file_source`, `operation.rs`). Stage and commit
  therefore store the content the mount serves. A path holding no file, nothing or a directory,
  answers `content_source` with `FileNotFound`, which `infer_is_diffable` and the default
  `infer_is_conflicted` take as neither diffable nor conflicted.
- **Instance writes.** The `InstanceOperation` write methods, which reset, merge and conflict
  materialization call, run the provider's own request handlers by path, as the same requests
  through the kernel would (`operation.rs`, `Looked`, `write_content`): a lookup per name, held
  until the write ends; CREATE, MKDIR, truncate and WRITE for `write_file`, `write_node` and
  `set_file_to_immutable_store_contents`, after an UNLINK of a symlink at the path, which is
  never written through (`emptied_file`; only a reset reaches one, lore-as0); SETATTR for
  `make_executable`; UNLINK and RMDIR, recording deletions, for `remove` and
  `remove_recursive`, where a missing path is no error,
  as on an ordinary clone; RENAME for `rename`, child by child onto an existing directory and
  for a tracked directory, which RENAME refuses with `EXDEV`, except a change of case alone,
  which respells the directory in place and its host directory with it, made where none was
  so the next mount takes the spelling. `copy_file` keeps the source's executable bit. Stored
  content is staged under `write-staging/` and copied in 1 MiB WRITEs. Each changed name and file
  is then invalidated in the kernel. A written file is a dirty local file with the zero address,
  as a user's write leaves it, and stays dirty until a commit stores it (plan decision 9 as built;
  follow-up lore-ew0, Remaining work), so a merge's files survive a remount through `write/` and
  its deletions through the deletion list. A `write_node` of the content the mounted revision
  holds at the path, over the revision's own node there, as a reset of an edited file asks,
  instead turns the file back to tracked content served from the store, as a forced sync's
  overwrite does (`restore_revision_content`, `overwrite_local_file`). A node made at the path
  since, by a create or a rename onto it, and a path the deletion list holds, as after a rename
  away and back, take the write path, whose copy under `write/` the next mount puts back over the
  deletion. The operation reads the deletion list once, and again only once an append or rewrite
  has moved its change count (`DeletionList::changes`, `deletion_listed`). A provider without
  `write/` answers `NotSupported` (`FuseOperation::writable`, `operation.rs`).
- **Desktop trash.** GNOME's `gvfsd-trash` treats every lore mount as a volume of its own and
  looks up `.Trash` and `.Trash-<uid>` in its root
  (`loading_a_fuse_instance_never_enters_the_mount`, `lore-revision/tests/fuse_mount.rs`), and
  any file created there, which a debug build's unmount invalidates again. A
  file-manager delete in a virtual clone may therefore create `.Trash-<uid>` at the repository
  root, where status shows it; an ordinary clone uses `~/.local/share/Trash`. The `x-gvfs-notrash`
  mount option would opt out, but `fusermount3` fails to set it (lore-9fr).
- **STATFS** adds the size of the file system holding `write/` and reports its free blocks and
  inodes (`statfs_with_local`, `write.rs`).

### Inodes

- **Derived numbers.** `xxh3(tag, parent inode, folded name hash)`, with a tag per kind (directory,
  file, symlink), never 0 or 1 (`derive_inode_number`, `fs/fuse/inode_number.rs`). Numbers
  survive remounts and service restarts without a persisted counter. A collision with a live entry
  of another identity rehashes (`rehash`, `inode_number.rs`; `InodeTable::insert_child_if`,
  `inode.rs`).
- **Stable inodes.** Content is no part of a number, so a file keeps its inode through edits,
  copy-up, commits and syncs, as on ext4: each rewrites the entry in place (`flip_to_local`,
  `flip_held`, `write.rs`; `swap_through`, `swap.rs`), and whoever changes a
  file's content sends `inval_inode` for it. RENAME moves the entry and keeps its number, over
  the replaced entry in one critical section (`InodeTable::move_child`, `inode.rs`). A
  directory keeps its inode while its path exists.
- **Orphans.** A removed, replaced or mismatched inode is orphaned: unindexed, it lives on for
  open handles until FORGET (`InodeTable::orphan`, `inode.rs`). A lookup revives a file orphan
  only for the same store-served content, address and executable bit; directory and local-file
  orphans are never revived. Like a new entry, a revival, and a live entry a lookup finds under
  the child's derived number, are counted only where the lookup's admission agrees: no swap
  since, no settle of the name, and the node still holding the name (`reuse`, `inode.rs`).
- **Tables.** A `DashMap` inode table plus a child index keyed by (parent, folded name hash). The
  entry lock linearizes a counted lookup against FORGET; an entry lock may be held while a child
  index lock is taken, never the reverse (`InodeTable`, `fs/fuse/inode.rs`).
- **Entry state for writes** (`InodeEntry`, `inode.rs`): the write generation WRITE, SETATTR
  and FALLOCATE bracket; the incarnation, drawn from a table-wide count so a number FORGET freed
  and a lookup took again is a new entry; the host epoch and `host_file` that tie the descriptor
  cache to the inode's own host file; a directory's renames under way and their change counter;
  and the State node it stands for. A FORGET of a written file stamps that node's slot, which a
  commit's flip of a file no inode held compares (`written_forgotten_since`, `inode.rs`).
- **Identity check.** LOOKUP and READDIRPLUS match an existing entry against the State: a
  directory must stand for the same node and a symlink stay a symlink; a file matches whatever its
  node, takes the node where no swap came between, and takes address, size and executable only
  while both sides are store-served. An entry of another node or kind is checked against the
  node's record, read again: where the node still holds the name, a file takes the node only as
  the one inode indexed under the name's hash, or where a pass over several found none standing
  for the node, and the rest are mismatches; where it no longer does, a RENAME over the name
  moved the entry there since the record was read, and it answers as it stands. Any entry at the
  name that matches is taken, a mismatch answering only where none matches. A mismatch while a
  request moving a node to the name is under way waits for one release of its claim; otherwise
  it is orphaned and a fresh inode made, so correctness does not depend on the swap walk
  (`identity_refresh`, `child_matching`, `child_inode`, `added_child`,
  `filesystem.rs`). READDIRPLUS reports the type of the inode it answers.
- **READDIR numbers** are derived without creating inodes (`readdir_ino`, `filesystem.rs`):
  after a rename or a rehash the derived number differs from the live one until the next lookup.

### Name lookup and the spelling record

- Listings are sorted by (folded name hash, name). `Listing::find` binary-searches the hash and
  fold-compares only the tied run (`fs/fuse/listing.rs`). It allocates nothing for an ASCII
  name of up to 64 bytes; past that `hash_string` lowercases the name into a `String`, and
  `fold_eq` does so for a pair holding a non-ASCII name.
- The State holds one spelling per folded name, and LOOKUP answers a name only in that spelling;
  any other answers absent, as on an ordinary Linux clone (`LookupCase::Exact`,
  `fs/fuse/filesystem.rs`). Were two spellings to answer with one inode, the kernel would take a
  rename between them for a rename onto itself and send nothing (`vfs_rename`), so `mv Foo foo`
  would leave `Foo`. A CREATE, MKDIR or MKNOD of a name that folds to a held one answers `EEXIST`.
  Requests made on lore's behalf, and write requests resolving a name they act on, match any
  spelling (`LookupCase::Folded`).
- The kernel's dcache is case-sensitive, so each spelling looked up becomes its own dentry. The
  spelling record keeps every spelling the kernel may cache per child key, positive or negative,
  so a swap invalidates exactly those dentries, the cached absence of a spelling it respells a
  name to included (`fs/fuse/spelling.rs`).
- Positive entries and attributes live 3600 s, negative entries 60 s. Past 8 spellings for one
  key, every reply for that key carries zero timeouts.

### Listings and the listing cache

- A listing is one 80-byte entry array plus one name arena, with the node id of each entry
  (`Listing`, `ListingEntry`, `listing.rs`). View-excluded children stay listed with a
  flag, so LOOKUP answers `ENOENT` without a second filter pass. A local change outside the view
  stays listed, as an ordinary working tree keeps a local file its view leaves out
  (`holds_local`). Each entry, and each directory's inode, keeps the line that decided its view
  verdict beside the packed bits, so a listing below an excluded directory resumes from that
  line: a re-inclusion written before it re-includes nothing below, as in an ordinary clone
  (`view::view_line`). A view change recomputes every directory inode's verdict under the new
  view (`plan_view_change`). Local entries are listed from the
  State with the `LOCAL` flag; there is no second listing to merge.
- **Changed directories.** A listing records its directory's chain log position when its build
  began; the State primitives that attach and detach append a patch to that log under the block
  write lock they already hold (`NodeBlock::watch_chain`, `state/block.rs`).
  - A lookup reads a listed candidate's record and name under its block's read lock. Where the
    node still holds the name under the listed spelling it answers with no log read, so no change
    since the build makes a listed name stale. Where it holds the name under another spelling,
    say renamed in case or away and back, it answers through its block with that spelling
    (`Listing::lookup`, `listing.rs`).
  - Otherwise the lookup scans the patches since the build, newest first: an attach answers
    through the block the patch names, a detach or nothing answers a miss, and a log that no
    longer holds the listing's position, a respelling or a dropped block answers stale, which
    rebuilds.
  - A log holds 16 patches from its directory's first listing. A build that finds it full grows
    it fourfold, to 64, 256 and at most 1,024 patches, keeping each patch at its sequence, so a
    busy directory rebuilds once per 1,024 changes (`NodeBlock::grow_chain_log`, `state/block.rs`).
    Only a lookup of a name its listing does not hold scans the log, up to 1,024 compares. A
    directory whose log is at 1,024 holds 24.6 KiB of it: 16 B of slot, 32 B of header and 24 B a
    patch (`NodeBlock::chain_log_bytes`). A reattach adds `REATTACH_CHANGES` (2^16) to its old parent's count, which
    a walk by name reads to start again (`state.rs`); a walk that may have repeated a moved
    node drops the repeats (`drop_repeated_nodes`, `state.rs`).
  - Nothing drops a listing but a view change, which publishes a fresh cache (`swap_through`,
    `swap.rs`). A copy-up, a commit's flip and settle, and a swap's content change change records in
    place: every reader takes a listed entry with its node's record as it stands, and one whose node
    left the name as absent (`Listing::current_entry`, `listing.rs`). UNLINK, RMDIR, RENAME and a
    swap's attaches and detaches are patches in the log; a RENAME over a name logs the holder's
    detach and the mover's attach in one critical section, so a lookup never misses the name.
  - A READDIRPLUS stream reads on with attributes until its directory takes a detach or a
    respelling, then lists plain (`Listing::rows_current`, `listing.rs`); a create keeps it
    plus. A swap since the pin makes it plain too, from the row that sees the swap begin. A row
    refused an inode, whose node left its name or whose lookup fails is plain alone, and the rows
    after it keep their attributes (`push_dirents_plus`).
- The cache is keyed by the directory's node id in the filesystem State
  (`fs/fuse/listing_cache.rs`). Two budgets bound it apart, and eviction runs while either is
  exceeded: 256 MiB of the listings' own bytes, and 1 GiB of the node blocks cached listings hold.
  The blocks had shared the listings' budget until a cold ShaderCompileWorker build filled it with
  257 MB of blocks against 16 MB of listings, evicting 43,000 listings that UnrealBuildTool's
  makefile phase then rebuilt. A listing holds its directory's block and every block holding one
  of its children, so an unchanged directory is answered under block read locks alone; such a
  block, about 50 KiB of nodes plus its name table, stays in memory while any cached listing holds
  it and is charged once, by the first listing to hold it, at what it held then, which the last
  one to leave the cache gives back.
- A hit takes one `DashMap` shard read lock and marks the entry referenced, writing only when the
  bit is clear. Eviction is CLOCK over a lock-free queue, one evicting thread at a time
  (`fs/fuse/clock.rs`).
- READDIR reports derived inode numbers without creating inodes (`push_dirents`,
  `filesystem.rs`). READDIRPLUS creates an inode and counts a lookup per entry it lists with
  a node id.
- **One build per directory.** Concurrent requests that need the same uncached listing share one
  build: a `DashMap` of `OnceCell`s keyed by the directory's node id (`build_listing`,
  `filesystem.rs`). A build is cached even where a swap or a reattach ran during it: its log
  position precedes its walk, so a lookup replays what landed since. Kernel requests and path
  walks (`listing_at_path`, `entry_at_path`, the recording prefetch and
  `FuseOperation::read_directory`) all go through it. Each sharer holds a guard; the last one to
  drop, whether its build finished or was cancelled by an INTERRUPT, removes the entry.
  Concurrent listings of different directories build side by side, off the workers.
- **Cold build cost.** A directory's children are a sibling chain across node blocks, and children
  added over many revisions sit in blocks far apart (3-41 blocks per directory at depths 1-6 in
  the measured tree). Each block is known only once the one before it is loaded, so the blocks load
  in turn; each block's name table loads alongside the rest of the walk, once per block even when
  the chain returns to it (`State::load_children_with_nametables`,
  `lore-revision/src/state.rs`). A directory spanning K uncached blocks costs K + 1 round
  trips.
- Loading a node block starts no other work: a command that reads a block's file metadata fetches it
  then (`State::block_file_metadata`, `state.rs`), and a clone's tree prefetch warms
  file-metadata blocks on its own awaited path. A task left running past a command would hold the
  command's repository, which the service's end-of-command check forbids.
- **File metadata and new nodes.** The filesystem State takes new nodes from fresh blocks past the
  mounted revision's, never from the free slots its blocks hold
  (`State::take_new_nodes_from_fresh_blocks`, called by `fresh_state` in `filesystem.rs`). The
  State is never serialized, so reusing those slots would only cost a stored node block, its name
  table and its file-metadata block, about one remote read per 18 creates. A fresh block holds its
  name table in memory and has no stored file metadata, so a CREATE, MKNOD, MKDIR or SYMLINK takes
  its slot reading nothing from the store, from the first one after a mount on
  (`State::attach_slot_resident` holds). The cost is one 512-node block per 512 creates. The
  deferred slot load (`grab_node_slot`, `state/node/slots.rs`) stays for a State that takes slots
  from its unused chain.

### Attributes and times

- Files are `0444`, or `0555` if executable; directories `0555` with `st_nlink = 1`. A writable
  provider adds the owner write bit. Owner is the mounting user; `st_blksize` is `MAX_READ`
  (1 MiB) (`attr_for`, `fs/fuse/attr.rs`). Local entries report their host file (see Writes).
- Every path reports the instance's base time until a swap records one.
- Swap times are stored under their own mutable-store keys (`fuse-recorded-time`, with a
  `fuse-recorded-times` marker), apart from the file modification keys scans and commits write.
  Until the marker exists no inode creation reads the store (`RecordedTimes`, `attr.rs`).
- Times are stored before a swap applies anything (`swap_through`, `swap.rs`).

### The READ path

`FuseFilesystem::read` → `read::read_file` → `FragmentCache::read` (`filesystem.rs`,
`fs/fuse/read.rs`, `fs/fuse/fragment_cache.rs`).

**Local first, then remote with a deadline.**
- `fetch: false` reads local fragments only. A missing address or payload returns `WouldFetch`,
  which the dispatcher defers.
- `fetch: true` runs under a deadline, 60 s by default. A miss returns `EIO` and records the
  address. For 5 s afterwards, READs of that address fail at once, because the kernel retries a
  failed page read once (`RecentMisses`, `read.rs`). The record holds at most 4096 addresses.
- With no remote session (offline), a fetch fails with `AddressNotFound`, returned as `EIO`
  without waiting on the deadline.
- Remote fetches take turns over every storage connection the service opened: the read session
  resolves to a pooled session that runs each request on the next connection
  (`resolve_session`, `lore-revision/src/immutable.rs`; `pooled`, `lore-transport/src/session.rs`).
  The server paces each QUIC connection on its own, so over a distant path throughput follows the
  connection count.
- Readahead, the prefetch and a command's state caching go through the same lazy session, the
  repository's (`immutable::cache_session`, `lore-revision/src/immutable.rs`). It starts its
  server-side sessions again once a fetch invalidates them, as a QUIC reconnect leaves the server's
  session ids stale.
- **Whole-file readahead.** The first READ that continues the previous one on an inode starts a
  background `immutable::cache` of the whole file, once per inode, for files over one fragment
  (256 KiB) and up to 64 MiB (`note_read`, `start_readahead`, `read.rs`).

**Verification** ([ADR-00023](../docs/developing/decisions/00023-data-verification-and-trust.md)).
- Local bytes are served as stored and never hashed; only their sizes are checked against their
  fragment (`check_payload_sizes`, `lore-storage/src/read.rs`). The provider context keeps
  caching on (`provider_context`, `filesystem.rs`).
- Every payload a read, readahead, the prefetch or a command's state caching fetches is checked
  on receipt (`check_receipt`, `lore-storage/src/read.rs`; `immutable::cache_counted`,
  `lore-revision/src/immutable.rs`): decompressed and hashed against its address, unless the
  command declares the remote trusted (`--trust-remote`), which checks its sizes alone. One that
  fails is healed once where the server heals it, and is otherwise an error, never stored.
- Local bytes are trusted because nothing unchecked reaches the store the provider serves from.
  `LORE_IMMUTABLE_STORE_VERIFY_WRITE` adds a read-back check on local store writes.

**Fragment cache strategies** (`FragmentCacheStrategy`, `fragment_cache.rs`). Bench labels in
parentheses:

| Strategy | Keeps |
|---|---|
| `Storage` (S0) | Nothing: the storage walk into an intermediate buffer. |
| `PerInode(Load)` (S1) | Per-inode list levels; leaves reload every READ. |
| `PerInode(Decoded(n))` (S2; S3, default) | Lists and the last `n` decoded leaves, compressed or not. |
| `PerInode(Stored(n))` (S5, S5x4) | Lists and the last `n` leaves as stored; each READ decodes a compressed leaf whole. |
| `Global { budget, decoded }` (S4…) | One byte-budgeted cache of lists and leaves keyed by address. |

`n` is clamped to 4. Per-inode state is keyed by inode, checked against the inode's root address,
dropped on FORGET, and evicted by CLOCK when all inodes together exceed 64 MiB. A leaf loads whole
and decoded once, and later READs inside it copy from memory. A file of one fragment keeps nothing:
each READ loads it. A READ the caches serve whole finishes before its first await, and allocates
nothing (`hot_read_of_an_uncompressed_fragmented_file_allocates_zero`,
`lore-revision/tests/fuse_allocations.rs`).

**Parallel leaf loads.** What a READ still needs loads side by side: one job runs inline, several
run as tasks in a `JoinSet`. Tasks own their buffers, so a deadline or INTERRUPT that drops the
READ leaves them nothing of the caller's (`fetch_all`, `fragment_cache.rs`).

**Sequential prefetch** (off by default, per-inode strategies only; `fragment_cache/prefetch.rs`).
- A stream starts after 2 READs in a row begin where the previous ended. READs within
  max(256 KiB, twice the READ length) of the cursor keep the stream; others break it.
- A task per stream loads whole decoded leaves ahead into a 64-cell ring. The window starts at
  512 KiB and doubles per batch up to 2 MiB.
- Prefetched leaves hold at most a quarter of the inode budget and never evict demand-loaded state.
- It applies to local reads only (`FragmentCache::read`, `fragment_cache.rs`).

### Concurrency model

- Hit paths take no exclusive lock. A fragment-cache hit takes a `DashMap` shard read lock, pins a
  crossbeam epoch and loads a pointer; replaced entries are reclaimed by the epoch collector, with
  displaced payloads of 64 KiB or more flushed at once (`FLUSH_DISPLACED`, `cell_swap`,
  `fragment_cache.rs`).
- Byte budgets are exact: every entry is counted in before the swap that makes it visible
  (`FragmentCache::install`, `fragment_cache.rs`).
- CLOCK eviction is shared by the listing cache and the fragment cache: a hit sets a referenced
  bit; the hand spares a marked entry once.

### Error mapping

| Condition | Result |
|---|---|
| Modifying opcode on a provider without `write/` | `EROFS` (`default_errno`, `src/filesystem.rs`; `FuseFilesystem::writable`, `fs/fuse/write.rs`); an `ro` mount stops these first. |
| `InstanceOperation` write on a FUSE instance without `write/` | `NotSupported` (`writable`, `fs/fuse/operation.rs`) |
| A local file's descriptor that cannot be opened as the inode's own host file in 8 attempts | `EIO` (`descriptor`, `fs/fuse/write.rs`) |
| xattr, copy_file_range, tmpfile, flush, fsyncdir, lseek, poll, bmap, statx, access, locks; fsync without `write/` | `ENOSYS`, which the kernel remembers per connection; it copies a `copy_file_range(2)` with splice and answers `EOPNOTSUPP` to xattr writes and `O_TMPFILE` |
| `READLINK` of anything but a local symlink / `IOCTL` | `EINVAL` / `ENOTTY` |
| `LINK` on a writable provider / renaming a tracked directory | `EPERM` / `EXDEV` |
| Local read failure, remote failure, missed deadline, fail-fast window | `EIO` (`read_file`, `read.rs`) |
| INTERRUPT wins the race | `EINTR` |
| Name over 255 bytes, or another body that does not decode | `ENAMETOOLONG` for a name, `EIO` otherwise; FORGET gets no reply (`src/session.rs`, `answer_undecodable`) |

FUSE adds one Lore error code, `MountsBusy` (51). The protocol layer's errors (`lore-fuse`:
`FuseError`, `DecodeError`, `TransportError`) carry none: a decode failure becomes an errno for
the kernel, and a mount, unmount or INIT failure leaves the provider as `Internal` with its
context. What crosses the `FilesystemProvider` boundary uses Lore's codes: a write refused by a
read-only instance, and a mount of a revision holding a link, are `NotSupported`.

### Access recording

Milestone 2, decision 3: a mount can record the first access to every path it serves, input for a
later `lore clone --prefetch <file>`. A file's first successful non-empty READ of tracked content
records the file (`record_file_access`, `filesystem.rs`); a READ of a local file
records nothing, since its content is local. A LOOKUP (positive or negative) records its parent
directory, and a READDIR/READDIRPLUS the directory it lists, local and fetching forms alike, once
the listing that answers is at hand (`lookup_in`, `pin_stream`, `filesystem.rs`). The path is
resolved from the inode table's parent/name chain alone, the same in-memory walk `path_of` does, no
store I/O; directories end with `/`, the root is `/`. A directory recorded with its listing carries,
after a tab, the ascending index of every node block holding one of its children, taken from the
listing's node ids (`record_dir_access`, `filesystem.rs`): the blocks its sibling chain spans, which
a prefetch can then fetch at once instead of in turn.

Each inode entry gates its own first access with its `access_recorded` flag through `claim_once`
(`claim_once`, `inode.rs`), the gate readahead's `note_read` uses: a relaxed load, then one
compare-exchange. Only the winner resolves a path and sends it; with recording off, the call
costs one `OnceLock::get()` and returns (`claim_access`, `filesystem.rs`), no atomic
touched, nothing allocated.

Winners send the line over an unbounded channel to one writer task that appends lines through
an async `BufWriter` over a `tokio::fs::File`, flushing every 250 ms and once more when the
sender side drops (`FLUSH_INTERVAL`, `write_loop`, `access_recorder.rs`), so the file can be read
while the mount runs. The format is UTF-8, one path per line, first-access order (best effort under
concurrency); a path can recur after its inode is forgotten and recreated, which the consumer is
expected to dedup. A recording of a ShaderCompileWorker build holds 50,413 paths, 36,282 directories
with block indices naming 5,497 of the tree's 44,196 node blocks.

Recording is off unless the service's environment has `LORE_FUSE_RECORD_DIR=<dir>`, read once
where the mount is constructed (`mount_with`, `fs/fuse/mount.rs`); each mount then writes to
`<dir>/<instance id>-<unix seconds>.txt` (`AccessRecorder::from_env`, `record_file_path`,
`access_recorder.rs`).

### Prefetch

Milestone 2, decisions 2 and 4. `clone_materialize_hold` hands the provider the revision, starts
one background task on the provider and returns (`repository/clone.rs`;
`FuseFilesystem::start_prefetch`, `prefetch.rs`). The task warms the store the mount reads:
the access recording given with `--prefetch <file>` first, then the tree. Each part logs one
summary line to the service log: entries, MiB and seconds. The recording's paths and the tree
report MiB fetched, the process's remote fetch count over the run
(`lore_storage::remote_fetch_bytes`, `lore-storage/src/read.rs`), so it includes foreground
fetches made meanwhile; the recorded blocks report the content of the node blocks they read, the
payload of the name tables and file-metadata blocks they fetched, and their failed fetches. A block
warm-up also logs one debug line of its own; the cache runs inside the warm-ups log none.

- **Recording.** The clone reads the file when it publishes, so a file that cannot be read is a
  warning to the command (`plan`, `repository/prefetch.rs`). Paths are spelled as the line holds
  them and kept in first-access order, each once as parsed; `/` is the root, a trailing `/` marks a
  directory, and a line that is not UTF-8 or holds no relative path is skipped. Block indices
  after a tab on any line are kept in first-need order, each once (`parse_recording`,
  `repository/prefetch.rs`). Two parts run at once:
  - Recorded blocks: the node blocks the indices name, as `blocks` fetches them, in the
    recording's order (`prefetch_recorded_blocks`, `prefetch.rs`). An index is looked up in
    the served revision's block address list, so a recording of another revision fetches
    whatever blocks now sit at its indices, and listings still build from the chains.
  - Paths (`warm_recording`, `prefetch.rs`), through the provider's own `listing_at_path` and
    `entry_at_path` (`filesystem.rs`), as a LOOKUP would, sharing listing builds with
    kernel requests: every recorded directory's listing and every recorded file's ancestors'
    listings are built, and kept where the listing cache takes them. Paths start in recorded
    order, 2,048 resolving at a time, so one slow listing holds up no other. Each resolved file's
    content is cached through the repository's lazy session, taken once, in the order files
    resolve, 512 files at a time; a fetch that fails counts as failed. Paths absent from the
    revision, excluded by the view, or of the other kind are skipped. A swap stops new paths from
    starting.
- **Tree**, chosen by `LORE_TREE_PREFETCH_DEPTH` in the service's environment
  (`TREE_PREFETCH_ENV`, `tree_prefetch_from_env`, `repository/prefetch.rs`):
  - `blocks`, the default: every node block, its name table and its file-metadata block, the
    addresses taken from the block address lists (`State::block_address_lists`, `state.rs`).
    Nothing is walked and no view applies. Every fetch goes through the repository's lazy
    session, taken once, 512 blocks in flight, each node block fetched with its file-metadata block
    and then its name table, the next starting as one ends (`warm_blocks`,
    `repository/prefetch.rs`). A node block is read whole, from the local store where it holds
    it, and otherwise under a cache get permit, fetched and stored; its name table address is read
    from its content, so it is expanded once. A fetch that fails is counted, and a failed node
    block's name table is not fetched.
  - `<n>`: the clone's discovery walk with the view filter (`BlockDiscoverDispatcher`,
    `repository/clone.rs`), entering directories down to `n` levels below the root, so that
    every directory above that depth lists locally. The walk follows every node's sibling link, so
    every block holding an entry of a directory it enters loads, and reads and filters the names
    of directories alone. Blocks load through `block_with_nametable`, as a listing loads them, at
    most 64 at a time (`warm_tree`, `repository/clone.rs`). `all` walks the whole view; `0`
    turns the tree part off.
  - Once a swap supersedes the revision the prefetch started on, `blocks` starts no further block
    and the walk no further load (`prefetch_tree`, `prefetch.rs`; `StopWhen`,
    `repository/prefetch.rs`).
- **Cancellation.** The provider keeps each prefetch task's abort handle; an unmount aborts them
  after the session stops (`stop_prefetch`, `prefetch.rs`).
- **Priority.** The walk's loads are the listing path's priority reads, and so are a READ's
  fetches (`read_options`, `fragment_cache.rs`). `immutable::cache` fetches at normal
  priority, which QUIC keeps off the streams it reserves for priority requests
  (`send_normal`, `lore-transport/src/quic/client.rs`), so a READ's fetch does not share a stream
  with the prefetch's.
- **Completion.** `prefetch_running` and `prefetch_finished` (`prefetch.rs`) read an
  atomic count and a `Notify`. No request path touches them.

Measured on 2026-09-25 against `lore.internal.epicgames.net/Fortnite`, revision `fe9b2325…`, the
Linux view, a cold isolated store each run, one storage connection (before r1346) and the prefetch
before r1351 (block batches with barriers, 256 paths resolving, contents in batches of 256). The
SD-WAN link (`sdwan0`) carried about 1.6 MiB/s of other traffic before the runs and about 3 MiB/s
in all during them (a real clone and other services).

| Setting | Clone returns | Prefetch | Fetched | What |
|---|---|---|---|---|
| tree `2` | 1.90 s | 19.7 s | 22.0 MiB | 47 directories, 21 files, 85 blocks |
| tree `blocks` | 1.73 to 2.01 s | 303 to 349 s (3 runs) | 1070 to 1076 MiB | 44,196 node blocks (895 MiB), 33,713 name tables (113 MiB), 44,196 file-metadata blocks (66 MiB) |
| tree `all` | 1.89 s | 449 s | 793.2 MiB | 3,527,418 directories, 5,963,019 files, 26,019 blocks |
| tree `0`, recording of a ShaderCompileWorker build | 1.92 s | 304.6 s | 512.2 MiB | 50,429 paths: 35,773 directories and 13,710 files (312.3 MiB of content) warmed, 946 skipped |

- The 946 skipped paths are absent from the revision (build outputs such as `…/obj/`), checked
  through the mount.
- After `blocks`, a first `stat` of a path 6 to 9 levels deep took 40 to 95 ms, with no fetch.
- With 16 connections and the prefetch of r1351, during a build: a recording's blocks 9-11 s,
  its paths 19-21 s, `blocks` 27-34 s ("Cold build against the remote").

## Knobs

No knob has a config key; only the rows naming an environment variable or a CLI flag can be set
without code. The mount the service creates uses every default, read-write
(`FuseMount::mount_writable`, `mount_with`, `fs/fuse/mount.rs`). `fuse.enable_uring` does not appear
in the code.

| Knob | Where set | Default | Effect |
|---|---|---|---|
| `FuseFilesystem::set_sequential_prefetch` | API (`filesystem.rs`) | off | Sequential prefetch, above. |
| `FuseFilesystem::set_readdirplus_plain_first` | API (`filesystem.rs`) | off | READDIRPLUS lists a directory as READDIR does until one of its files is looked up: `find` creates no file inodes, `ls -l` looks each file up. |
| `FuseFilesystem::set_read_deadline` | API (`filesystem.rs`) | `FETCH_DEADLINE` = 60 s (`read.rs`) | Deadline on a remote fetch. |
| `OpenMode` | API, `FuseFilesystem::new_with_open` (`filesystem.rs`) | `ZeroMessage` | `KeepCache` answers OPEN with `FOPEN_KEEP_CACHE \| FOPEN_NOFLUSH`, handle 0. |
| `FragmentCache::set_strategy` | API (`fragment_cache.rs`) | `DEFAULT_FRAGMENT_CACHE` = `PerInode(Decoded(4))` | Strategy table above. Drops everything held; for an idle provider. |
| `FragmentCache::with_inode_budget` | API (`fragment_cache.rs`) | `INODE_BUDGET` = 64 MiB | Cap on all per-inode state. |
| `MAX_LEAVES` | constant (`fragment_cache.rs`) | 4 | Clamp on leaves kept per inode. |
| `LISTING_CACHE_BYTES` | constant (`listing_cache.rs`) | 256 MiB | Listing cache budget for the listings' own bytes. |
| `LISTING_BLOCK_BYTES` | constant (`listing_cache.rs`) | 1 GiB | Listing cache budget for the node blocks cached listings hold in memory, each counted once. A cold ShaderCompileWorker build holds 326 MB. |
| `FAIL_FAST_WINDOW` | constant (`read.rs`) | 5 s | How long a missed address fails at once. |
| `RECENT_MISSES_MAX` | constant (`read.rs`) | 4096 | Addresses the fail-fast record holds. |
| `READAHEAD_WHOLE_FILE_MAX` | constant (`read.rs`) | 64 MiB | Largest file whole-file readahead fetches. |
| `SEQUENTIAL_READS`, `SLACK` | constants (`fragment_cache/prefetch.rs`) | 2, 256 KiB | Stream start and break rules. |
| `AHEAD`, `FIRST_WINDOW`, `MAX_WINDOW` | constants (`fragment_cache/prefetch.rs`) | 64 cells, 512 KiB, 2 MiB | Prefetch ring and window. |
| `AHEAD_SHARE` | constant (`fragment_cache/prefetch.rs`) | 4 | Prefetched leaves hold at most 1/4 of the inode budget. |
| `SPELLING_BOUND` | constant (`spelling.rs`) | 8 | Spellings per key before zero timeouts. |
| `DEFAULT_TIMEOUTS`, `NEGATIVE_TIMEOUTS` | constants (`spelling.rs`) | 3600 s, 60 s | Kernel entry and attribute cache lifetimes. |
| `SessionConfig::workers` | API (`src/session.rs`) | min(available parallelism, 8) | Request-loop workers. |
| `InitConfig::max_pages` | API (`src/init.rs`) | 256 | Largest READ in pages; caps `max_readahead` and `max_write`. |
| `InitConfig::max_write` | API (`src/init.rs`) | 4096; `InitConfig::read_write()` 1 MiB | Largest WRITE; sizes each worker's request buffer. |
| `InitConfig::zero_message_open` | API | `false`; the service mount sets `true` | Offers `NO_OPEN_SUPPORT`. |
| `InitConfig::max_background`, `congestion_threshold` | API | 64, 48 | Kernel background request limits. |
| `MountOptions` | API (`src/mount.rs`) | `ro,default_permissions,max_read=…,fsname=lore:<id>,subtype=lore,noatime` | Mount option string. The `mount(2)` fallback adds `MS_NOSUID \| MS_NODEV`. |
| `MountAccess` | API, `mount_with_access` (`src/mount.rs`) | `ReadOnly`; the service `ReadWrite` | `ReadWrite` drops `ro` and `MS_RDONLY`. |
| `CACHED_FILES` | constant (`fs/fuse/write.rs`) | 16384 | Host files under `write/` held open before idle ones close, down to three quarters. |
| `DESCRIPTOR_ATTEMPTS` | constant (`fs/fuse/write.rs`) | 8 | Opens of a local file's descriptor before `EIO`, where each finds another host file or a changed inode. |
| `CHAIN_LOG_INITIAL` | constant (`lore-revision/src/state/block.rs`) | 16 | Patches a directory's chain log holds from its first listing, allocated by that build outside every lock and not charged to the block budget. |
| `CHAIN_LOG_GROWTH` | constant (`lore-revision/src/state/block.rs`) | 4 | How many times larger a build that finds a log full makes it: 16, 64, 256, then 1,024. |
| `CHAIN_LOG_GROWN` | constant (`lore-revision/src/state/block.rs`) | 64 | Patches it holds after its first grow. |
| `CHAIN_LOG_MAX` | constant (`lore-revision/src/state/block.rs`) | 1024 | Patches a log holds at most; a full log of this size stays, and its directory rebuilds once per 1,024 changes. |
| `STALE_REBUILDS` | constant (`fs/fuse/filesystem.rs`) | 4 | Rebuilds of a stale listing a lookup tries before it asks the State. |
| `REATTACH_CHANGES` | constant (`lore-revision/src/state.rs`) | 2^16 | What a reattach adds to a directory's change count, which a walk by name reads to tell a reattach from other changes. |
| `SETTLE_ATTEMPTS` | constant (`fs/fuse/filesystem.rs`) | 8 | Times a request that found a host file absent starts again after a settle. |
| `SETTLE_RING`, `SETTLES_BETWEEN_YIELDS` | constants (`fs/fuse/write.rs`) | 1024, 64 | Settled names the settle log keeps; settles between yields. |
| `FORGOTTEN_WRITE_SLOTS` | constant (`fs/fuse/inode.rs`) | 4096 | Slots, by node id, a FORGET of a written file stamps for a commit's flip. |
| `RENAME_WAITS` | constant (`fs/fuse/write.rs`) | 1024 | Yields a host entry made below a directory being renamed waits, at most, for the rename to end before it moves to the new host path. |
| `HOST_WRITE_ATTEMPTS` | constant (`fs/fuse/write.rs`) | 8 | Attempts a host write makes where a commit's flip or a rename above changes the file or its path under it. |
| `WRITE_DIR` | constant (`lore-revision/src/global/external_dir.rs`) | `write` | Directory of local content below the instance's external directory, which the service passes to `FuseMount::mount_writable`. |
| `DELETION_LIST` | constant (`fs/fuse/deletions.rs`) | `deleted` | Deletion list below the instance's external directory. |
| `STAGING_DIRECTORY` | constant (`fs/fuse/write.rs`) | `write-staging` | Staging directory beside `write/`. |
| `MAX_READ` | constant (`fs/fuse/mount.rs`) | 1 MiB | `max_read` and `st_blksize`. |
| `UNMOUNT_BUSY_RETRIES`, `UNMOUNT_BUSY_BACKOFF` | constants (`src/mount.rs`) | 5, 20 ms | Busy-unmount retries. |
| `DRAIN_WAIT` | constant (`fs/fuse/mount.rs`) | 2 s | Debug-build FORGET drain on plain unmount. |
| `DRAIN_RESEND`, `DRAIN_SETTLE` | constants (`fs/fuse/mount.rs`) | 50 ms, 100 ms | The drain's interval between invalidations, and how long the table must count no lookups before it ends. |
| `vfs.base_time` | instance config (`VfsConfig::base_time`, `lore-revision/src/repository.rs`) | clone time | Time every path reports until a swap records one. |
| `LORE_IMMUTABLE_STORE_VERIFY_WRITE` | env (`LocalImmutableStore::new_claimed`, `lore-storage/src/local/immutable_store.rs`) | off | `1` or `true` verifies each local store write by read-back and rehash. |
| `LORE_MAX_CONNECTIONS` | env of the service process, or `--max-connections` (`lore_globals_from_args`, `lore-client/src/cli/cli.rs`) | 1, at most 10 (`storage_connection_count`, `connect_impl`, `lore-transport/src/connection.rs`) | Storage connections the process opens; remote fetches spread over them. The first connection a process makes to a remote fixes the count for its lifetime. Throughput from a distant remote scales with the count ("Remote fetch throughput"). |
| `LORE_FUSE_RECORD_DIR` | env, read at mount time (`RECORD_DIR_ENV`, `access_recorder.rs`; `mount_with`, `fs/fuse/mount.rs`) | unset | Each mount records first-access paths to `<dir>/<instance id>-<unix seconds>.txt`. See "Access recording" above. |
| `LORE_TREE_PREFETCH_DEPTH` | env, read by each virtual clone in the service (`tree_prefetch_from_env`, `repository/prefetch.rs`) | `blocks` | `0` off, `<n>` levels, `all` the whole view, `blocks` every block. An unreadable value warns and takes the default. See "Prefetch". |
| `--prefetch <file>` | CLI, `CloneOptions::prefetch` (`repository/clone.rs`; `clone_impl`, `lore/src/repository.rs`) | none | Access recording a virtual clone warms. Resolved against the caller's working directory. |
| `CloneOptions::tree_prefetch` | API (`repository/clone.rs`) | `None`, the variable | Overrides `LORE_TREE_PREFETCH_DEPTH`. |
| `WARM_BLOCK_LOADS` | constant (`repository/clone.rs`) | 64 | Block loads a tree walk keeps in flight. |
| `BLOCKS_IN_FLIGHT` | constant (`repository/prefetch.rs`) | 512 | Node blocks `blocks` and a recording's block indices keep in flight, each with its file-metadata block and name table. |
| `RECORDING_RESOLVING` | constant (`prefetch.rs`) | 2048 | Recorded paths resolving at once. |
| `RECORDING_CONTENT_FETCHES` | constant (`prefetch.rs`) | 512 | Resolved files whose contents are fetching at once. |
| `CACHE_GETS` | constant (`lore-revision/src/immutable.rs`) | 512 | Gets `immutable::cache` has in flight on one connection to a remote: the recording prefetch, readahead and the `blocks` tree prefetch's block warm-up, and the cache calls commands await, which can queue behind a mount's backlog on the same remote: state caching in sync, clone, `info` and dump (`State::cache_fragments`), and the history lookup (`batch_load_history`, `find.rs`). The `Levels` and `all` tree prefetch read blocks at priority and do not take it, nor do demand reads; a `blocks` warm-up's node block takes it only where the local store lacks the block. A get goes to the remote at once, so with no cap a prefetch puts thousands ahead of a demand READ's priority get; on a slow link 100,000 or more queued. 512 is the smallest cap with no measured cost on a fast link (table below). Each connection holds its own permits (`Connection::cache_get_permits`, `lore-transport/src/connection.rs`), so a stalled remote holds back only the cache gets over its connections. A cache get that joins a read or fetch of the same content in flight gives its permit back. A get has no client deadline (`get`, `lore-transport/src/quic/storage_service/client.rs`): a remote that stops answering stops its own cache fetches until its held gets return, or the server's handler ceiling (1 h by default) ends them. A get that waited for its permit skips a fragment that turned local meanwhile. |
| `LORE_CACHE_GETS` | env of the service process, read and logged once (`cache_gets`, `immutable.rs`) | `CACHE_GETS` | Overrides the cap on each connection; `0` means no cap. A cap above what a tokio semaphore holds takes that maximum; a value that is not a count warns and takes the default. For measuring the cap on a given link. |

**`CACHE_GETS` on the fast link**, 2026-10-03: median of 3 cold builds per cap. Each build is the cold ShaderCompileWorker clone and build of the r1550 release binary through the quiet runner, on `sdwan0` (p95 40-51 MiB/s with no cap). Caps rotated per round, each set against its own no-cap runs.

| Cap | Median (s) | vs no cap |
|---|---|---|
| 64 | 186.7 | +14.7% |
| 128 | 169.3 | +4.1% |
| 256 | 167.2 | +1.4% |
| 512 | 164.6 | 0 |
| 0 | 164.9 / 162.7 | — |

The round trip of each get bounds the prefetch on this link, so throughput grows with the cap; at 64 the build outran the prefetch.

### Benchmark and test knobs

| Variable | Used by | Default | Effect |
|---|---|---|---|
| `LORE_TEST_SKIP_FUSE` | all mount tests (`SKIP_ENV_VAR`, `src/test_support.rs`) | unset | Skips every FUSE-gated test. |
| `LORE_BENCH_CONTENT` | `fs::fuse::read_cache_bench` | `random` | `random` or `compressible` (about 2:1 under zstd). |
| `LORE_BENCH_STRATEGIES` | `fs::fuse::read_cache_bench` | all | Comma-separated strategy labels. |
| `LORE_BENCH_W4_MB` | `fs::fuse::read_cache_bench` | 64 | MiB per W4 file. |
| `LORE_BENCH_FILES` | read-cache, `fs::fuse::bench`, `fs::fuse::vs_ext4_bench` | 100000 | Small files / tree size; 0 skips W5. |
| `LORE_BENCH_DISK_STORE` | `fs::fuse::read_cache_bench` | 0 | `1` puts the store in a temp directory instead of memory. Required by `bench_read_targets`. |
| `LORE_BENCH_PREFETCH` | `fs::fuse::read_cache_bench` | 0 | `1` turns sequential prefetch on. |
| `LORE_BENCH_NATIVE` | `bench_read_targets` | none | Native roots, `ext4=<dir>:zfs=<dir>`. |
| `LORE_BENCH_TARGETS`, `LORE_BENCH_WORKLOADS` | `bench_read_targets` | all | Comma-separated label prefixes. |
| `LORE_BENCH_DISK` | `fs::fuse::read_cache_bench` | none | Block device (as in `/proc/diskstats`) whose reads are reported. |
| `LORE_BENCH_PATTERN`, `LORE_BENCH_READ_KIB` | `bench_fragment_cache_read_breakdown` | in order, 128 | `random` scatters READs; READ size. |
| `LORE_BENCH_HOT_TASKS`, `_HOT_READS`, `_HOT_MIB` | `bench_hot_inode_reads` | 12, 200000, 64 | Tasks, READs per task, file size. |
| `LORE_BENCH_REPS` | `fs::fuse::bench`, `fs::fuse::vs_ext4_bench` | 5, 3 | Repetitions. |
| `LORE_COUNT_MIB` | the count diagnostics `per_read_operations_of_s0_and_the_default` (`lore-revision/tests/unit/fs/fuse/fragment_cache.rs`) and `sequential_read_counts` (`lore-revision/tests/unit/fs/fuse/fragment_cache/prefetch.rs`) | 64 | File size in MiB. |

Commands (all benchmarks are `#[ignore]`d; run one at a time):

```sh
LORE_BENCH_CONTENT=random cargo test --release -p lore-revision --test unit \
  -- --ignored --nocapture --test-threads=1 bench_fragment_cache_matrix

LORE_BENCH_DISK_STORE=1 LORE_BENCH_NATIVE=ext4=/mnt/ext4bench:zfs=<zfs dir> \
  cargo test --release -p lore-revision --test unit \
  -- --ignored --nocapture --test-threads=1 bench_read_targets

cargo test --release -p lore-revision --test unit \
  -- --ignored --nocapture bench_readdir_policy_find_and_ls_long
cargo test --release -p lore-revision --test unit -- --nocapture fs::fuse::counts::
cargo test --release -p lore-fuse --test unit \
  -- --ignored --nocapture --test-threads=1 protocol_bench::
```

Machine prerequisites for `bench_read_targets`:
- **Cache drop.** Passwordless `sudo -n /usr/sbin/sysctl -q vm.drop_caches=3`, for example a
  sudoers line `<user> ALL=(root) NOPASSWD: /usr/sbin/sysctl -q vm.drop_caches=3`. The bench
  asserts it succeeds (`drop_caches`,
  `lore-revision/tests/unit/fs/fuse/read_cache_bench.rs`).
- **ext4 target.** An ext4 file system on a loop image, mounted at `/mnt/ext4bench`. The bench
  checks the file system with `findmnt` and labels a loop device by its backing image.
- **Quiet run.** No other heavy CPU, disk or memory load during the run; the same CPU frequency
  policy across compared runs; enough free memory that warm runs read nothing from disk. A warm
  row with non-zero disk reads, or a spread far above its neighbours, was not quiet.

### Build benchmarks

`lore-fuse/bench/m2/` times a cold Unreal build on a virtual clone against the same build on an
ordinary clone of the same view and revision. Every script sources `common.sh`, which reads
`M2_ENV` (default `/tmp/lore-fuse-m2/env.sh`) for `LORE`, `LORE_GLOBAL_PATH`,
`LORE_SERVICE_SOCKET`, `LORE_SERVICE_EXECUTABLE`, `LINUX_MULTIARCH_ROOT` and `BENCH_REV`.
`M2_OUT` (default `/tmp/lore-fuse-m2/runs`) holds the output, `M2_CORES` (default `0-11`) names
the CPUs the build is pinned to, and `M2_ACTIONS` (default 12) is UBT's `-MaxParallelActions`.

| Script | What it does |
|---|---|
| `build.sh <tree> <target> <label>` | Waits for a quiet machine (no `cargo`, `rustc`, `clippy-driver`, `pytest` or server process, and under 5% CPU busy three checks running), then times `Engine/Build/BatchFiles/RunUBT.sh <target> Linux Development` pinned to `M2_CORES`, with UBA's store in the run directory so that no build warms the next. A foreign process seen during the run marks it `DISCARD`. |
| `cold-vfs.sh <view> <mount> [recording]` | Stops the benchmark service, told apart from other services of the same executable by its socket; deletes its shared store, external directories and store registry; drops the page cache; starts the service and clones `BENCH_REV` with `--vfs fuse --use-shared-store --view`, and `--prefetch` given a recording. `M2_KEEP_STORE=1` keeps the store. |
| `reset-real.sh <tree> [time]` | Returns the ordinary clone to its revision. Build outputs are mostly paths `.p4ignore.txt` ignores, which no scan reports, so it deletes everything changed since the marker the last reset left (since `time` the first time), runs `lore reset --purge`, and requires a clean `lore status --scan`. |
| `matrix.sh <real tree> <view> <target> <reps> [recording]` | `reps` ordinary builds, each after a reset and a cache drop, then `reps` cold virtual builds per mode (`vfs-tree`, and `vfs-recording` given a recording); one line per run in `M2_OUT/matrix.log`. |

A recording for `--prefetch` comes from one build on a virtual clone with `LORE_FUSE_RECORD_DIR`
in the service's environment ("Access recording").

`lore-fuse/bench/m3/` runs the milestone 3 measurements on top of `bench/m2/`. Its `common.sh`
sources the m2 one with `M2_OUT` defaulting to `/tmp/lore-fuse-m3/runs`. `BENCH_BIN` names the
lore binary under test, in place of `LORE` and `LORE_SERVICE_EXECUTABLE` from `M2_ENV`, and
`M3_MOUNT` (default `/tmp/lore-fuse-m3/mnt/fn`) is the cold clone's mount.

| Script | What it does |
|---|---|
| `quiet-run.sh <label> <command>...` | Waits until other processes have used under `QUIET_IDLE_TICKS` (50, half a core) in each of `QUIET_SECS` (30) seconds with no build or test process (`QUIET_FOREIGN`) running, then runs the command. The run is every process carrying its `QUIET_RUN_ID`, and their descendants. Another build or test process, others over `QUIET_BUSY_TICKS` (200) in one second, or over `QUIET_IDLE_TICKS` for 3 seconds kill the run, run `QUIET_CLEANUP` and start the wait again. Logs to `QUIET_DIR/<label>.log`, keeps the output as `<label>-run-<n>.out`, and gives up after `QUIET_DEADLINE_SECS` (36000) with exit 75. |
| `cold-build.sh <label>` | One cold virtual build for `quiet-run.sh`: `cold-vfs.sh` clones with the `M3_RECORDING` prefetch (none when set and empty), then `build.sh` builds `M3_TARGET` (`ShaderCompileWorker`) in it. Prints clone, build and total seconds; samples `M3_NET` (`sdwan0`) received bytes each second and records the service's `LORE_CACHE_GETS`. |
| `compare.sh <base> <new> [pairs] [tag]` | Cold builds of two binaries, alternated so both see the same link, each through `quiet-run.sh`; stops at the first failed build. |
| `caps.sh <binary> [rounds] [tag]` | Cold builds at each cache-get cap in `CAPS` (default `64 128 0`), rotated each round. |
| `verify.sh <tag>` | The milestone 3 verification set: clippy and rustfmt on the FUSE crates, the FUSE, State and command suites, the lore-fuse, lore-io and remote FUSE tests, a debug build, and the FUSE and service pytest suites. One `<step> exit=<status>` line per step. |
| `final-measure.sh <tag> <base>` | `verify.sh`, then the release binary, `bench/m2/latency.sh` three times and `compare.sh` against `base`, each step gating the next. |
| `fortnite-experiments.md` | How to time a Fortnite build from scratch on another machine: p4 sync, ordinary Lore clone, FUSE clone recording its accesses, FUSE clone prefetching from the recording. |
| `repeat-test.sh`, `contention.sh` | One test run over and over: alone, or in `LOOPS` (6) concurrent loops per test binary, every binary at once, to compare a fix against its base under the same load. |

## Measured performance

Machine: Intel i7-13800H, 30 GiB RAM, kernel 7.0. The FUSE store and the `zfs` target are on ZFS;
the `ext4` target is a loop image stored on ZFS. Dataset: 100,065 files including a 1 GiB file and
64 × 64 MiB files; median of 5 runs; 2026-09-24, revisions r1332 and r1335. FUSE runs the
default of the time, S6 (`PerInode(RangedDecoded(4))`: uncompressed leaves read in place and never
kept, since removed), with prefetch off unless stated.

| Workload, random content | FUSE S0 r1335 | FUSE S6 r1332 | FUSE S6 r1335 | ext4 | zfs |
|---|---|---|---|---|---|
| W1 seq 1 GiB, cold [GB/s] | 0.78 | 0.92 | 1.04 | 1.55 | 2.71 |
| W1 seq 1 GiB, warm [GB/s] | 12.76 | 13.36 | 11.92 | 1.82 | 3.62 |
| W2 rand 4 KiB O_DIRECT, warm [IOPS] | 10,845 | 21,495 | 17,449 | 58,568 | 3,946 |
| W3 rand 128 KiB O_DIRECT, warm [IOPS] | 7,002 | 8,222 | 10,269 | 20,149 | 3,096 |
| W5 small open+read, cold [files/s] | 11,774 | 12,648 | 12,434 | 82,024 | 14,335 |
| `find -type f`, cold [s] | 0.356 | 0.317 | 0.365 | 0.052 | 0.101 |
| statx lookup, warm [stat/s] | 520,130 | 641,001 | 560,923 | 626,698 | 476,075 |

- S6 against S0 (r1335, random): W2 warm +61%, W3 warm +47%, W4 cold +38%.
- Compressible content, S6 r1335 against r1332: W3 +78% to +83% in every mode; W1 cold +37%.
- Sequential prefetch (S6, r1335): W1 cold +132% random and +123% compressible; W4 remount (16
  concurrent streams) −28% random and −39% compressible.
- READDIR policy (100k files): plain-first `find` 86.6 ms against 230.1 ms; plain-first `ls -l` of
  ten directories 101.1 ms against 28.2 ms.

**Memory-pressure caveat.** During the r1335 run swap was 6–8 of 8 GiB used, and 7–10 GB of
shared memory outside any process or tmpfs was resident until mid-run. Of 125 FUSE W4 warm rows,
47 re-read the whole 4 GiB through FUSE; 38 of those coincided with more than 1 GiB of reclaim, and
no row with that much reclaim kept its page cache. W4 warm (±233% to ±810%), `find` warm (up to
±1377%) and every statx cell (±13% to ±39%) cannot be called in either direction.

**Cold LOOKUP against the remote** (`lore.internal.epicgames.net/Fortnite` at `fe9b2325…`, Linux
view, fresh store and service per run, QUIC RTT 135-141 ms, runs alternated back to back on
2026-09-24; a 147 GiB clone from the same remote ran throughout). Each step builds one cold
listing, the parent directory of the path stat'ed; wall seconds, two runs each:

| Step (listing built: blocks spanned / uncached) | Before (r1340) | After (r1343) |
|---|---|---|
| `ls` root (`/`: 12 / 12, plus the node block list) | 5.42, 5.90 | 2.72, 3.09 |
| `stat Engine/Source` (`Engine`: 10 / 9) | 2.66, 2.60 | 1.43, 1.49 |
| `stat Engine/Source/Runtime` (`Engine/Source`: 7 / 5) | 1.46, 1.48 | 0.84, 0.87 |
| `stat …/Runtime/Core` (`Runtime`: 41 / 39) | 11.36, 11.75 | 5.77, 6.02 |
| `stat …/Core/Public` (`Core`: 4 / 2) | 0.59, 0.74 | 0.46, 0.45 |
| `stat …/Public/CoreMinimal.h` (`Public`: 7 / 6) | 1.74, 1.81 | 1.07, 1.04 |
| `stat …/CoreUObject/Public/UObject/Object.h` (three listings: 3 / 1, 4 / 3, 27 / 26) | 8.66, 8.82 | 4.88, 4.81 |
| `cat CoreMinimal.h` | 0.15, 0.16 | 0.15, 0.15 |

An uncached block cost ~290 ms (node block, then its name table) and now ~150 ms. Probe,
verification, local put and session reuse add under 2 ms per fetch.

**Parallel cold access** (same remote, revision and view; 3,000 sampled `.h`/`.cpp` files under
`Engine/Source/Runtime` in 1,009 directories, 34.8 MiB; fresh store and service per run; one
thread per reader opening and reading each file; 2026-09-25, runs interleaved, `sdwan0` 1.4-4.3
MiB/s in every run). Files/s over the whole run:

| Readers | Workload | Before r1348 | r1348 |
|---|---|---|---|
| 64 | open + read, cold paths | 61.2, 63.7, 63.9 | 109.9, 112.1 |
| 128 | open + read, cold paths | 56.3, 56.6 | 145.4, 139.3 |
| 64 | `stat` all, then read all | 65.7, then 427.5 | 123.6, then 418.3 |
| 128 | `stat` all, then read all | 54.0, then 861.7 | 159.2, then 846.5 |

- Before, all 8 workers were inside a request in 73-86% of half-second samples, building
  listings for LOOKUP, while the kernel held every reader's request; remote fetches took one RTT
  (p50 142 ms) throughout. Cold READs with warm listings take one RTT at 64 and 128 readers
  before and after, so reads were never the limit.
- After, no worker is ever busy at a sample; up to one deferred request per reader waits on its
  fetch. Per-file latency p90 falls from 1.4-3.6 s to 0.4-1.1 s. What remains is the serial start
  (Remaining work) and one RTT per uncached block on each directory's chain.

**Cold build against the remote** (`lore.internal.epicgames.net/Fortnite` at `fe9b2325…`, the
Linux view of the standard view filter, `ShaderCompileWorker` Linux Development, 12 actions on the
P-cores, a UBA store per run; 2026-09-25, 16 storage connections, r1352). The ordinary clone was
cloned once and reset between builds; each virtual build starts from an empty store, and its
figures include the clone (1.9-2.1 s). Seconds:

| Build | Runs | Median | Outside UBA's compile | UnrealBuildTool compile |
|---|---|---|---|---|
| Ordinary clone | 156.3, 143.8, 149.9 | 149.9 | 21.0-21.8 | 12.6-13.2 |
| Virtual clone, `--prefetch` recording with block indices | 164.9, 169.1, 167.4 | 167.4 (1.12×) | 32.7-35.5 | 14.3-16.7 |
| Virtual clone over a store already holding everything | 153.3 | - | 26.4 | 14.7 |

- UBA's compile took 122.8-136.0 s across all runs, ordinary and virtual alike, most of the
  spread; the time outside it is what the clone changes.
- The recording's prefetch (5,497 blocks, 50,425 paths, 312 MiB of content, about 530 MiB in
  all) ended 19-21 s after the clone, its blocks alone after 9-11 s; the `blocks` tree prefetch
  then took 27-34 s for 893 MiB.
- Earlier steps, one run each: batched warm-up 191.1 s; contents fetched as files resolve 185.1
  s; 2,048 paths resolving 174.5 s; block indices 163.3 s, with READs at normal priority.
- Without a recording (`blocks` only) UnrealBuildTool took 84.8 s to compile and the makefile step
  was still running after 4 minutes: each source file waits one round trip.

### Milestone 3

**Warm LOOKUP while the tree changes.** These are the `lore-revision/tests/fuse_lookup_latency.rs` tests at r1531:
- the build: release, pinned to the P-cores (`taskset -c 0-11`), with `LORE_QUIET_MACHINE=1`;
- the workload: 2,000 LOOKUPs of names in one of four 2,000-entry directories;
- the runs: three, each gated on a quiet machine, on 2026-10-02.

The budget is p50 < 20 µs and p99 < 100 µs. The tests:
- under concurrent writes (`warm_lookups_under_concurrent_writes_stay_within_budget`), four writers create, write, rename and unlink in the measured directory and in others;
- during a sync (`warm_lookups_during_a_sync_stay_within_budget`), syncs of 400 changes run back to back;
- with saves only in other directories (`warm_lookups_with_saves_only_in_other_directories`);
- among creates in the measured directory (`warm_lookups_among_creates_in_their_directory`).

| Case | p50 | p99 | p99 of LOOKUPs that met no listing rebuild | LOOKUPs that met a rebuild |
|---|---|---|---|---|
| Saves only in other directories | 9.6-9.9 µs | 17-48 µs | 17-48 µs | 0% |
| Under concurrent writes | 9.5-9.7 µs | 1.26-1.31 ms | 130-160 µs | 2.8-5.3% |
| During a sync | 10.0-10.4 µs | 1.16-1.30 ms | 34-57 µs | 14-18% |
| Among creates in the directory | 9.5-10.0 µs | 0.41-0.43 ms | 0.36-0.39 ms | 0.7-0.9% |

- **The p50 budget holds everywhere. The p99 budget holds only where nothing changes the measured directory.**
- **Under writes and during a sync, the tail is listing rebuilds.** A LOOKUP that lands on a listing a RENAME, a copy-up or a swap dropped waits 1.0-1.8 ms for the rebuild. Without those LOOKUPs, during a sync's p99 is 34-57 µs. The chain log (lore-dev) removed these rebuilds; see Milestone 3 follow-ups below.
- **Among creates, the tail is the attached-since walk.** A name attached since its listing was built is found through the State's block lookup.

**Allocations per request** (`create_unlink_and_rename_stay_within_their_budget`, `lore-revision/tests/fuse_allocations.rs`, r1531):
- CREATE allocates 5, UNLINK 6 and RENAME 2, each exactly its budget.
- CREATE allocates the same over a tree that records file metadata
  (`create_over_a_tree_recording_file_metadata_stays_within_its_budget`).
- Warm LOOKUP, GETATTR, READDIRPLUS, OPEN, RELEASE, FORGET and STATFS allocate nothing. So do warm requests on local entries, READ and WRITE of an open local file, and a hot READ of a tracked file.
- A READ served from a local-store fragment allocates 3 (budget 8).

**Cold build against the remote.**
- **The setup is the milestone 2 one above**: the clone and build steps of `bench/m2/cold-vfs.sh`, run through `bench/m3/quiet-run.sh` (`bench/m3/compare.sh`).
- **Two versions, alternated on one link:** milestone 3 at r1536 and milestone 2 complete (r1429). The runs were on 2026-10-02.
- **The link:** `sdwan0` carried 12.5-12.6 MiB/s on average over each build's first 120 s, with 5 s peaks of 48-59 MiB/s.
- **Clone time is included.**

Seconds:

| Build | Runs | Median | Outside UBA's compile | UnrealBuildTool compile | Median against the ordinary clone's 149.9 s |
|---|---|---|---|---|---|
| Milestone 2 complete (r1429) | 159.5, 163.6, 165.1 | 163.6 | 30.5-32.9 | 14.1-14.6 | 1.09× |
| Milestone 3 (r1536) | 162.2, 165.1, 164.5 | 164.5 | 31.4-32.5 | 14.1-14.6 | 1.10× |

Milestone 3 and milestone 2 build within each other's spread on the same link, inside the 1.25× budget. UBA's compile took 128.9-132.9 s in every run. Three fixes got it there. Before them, milestone 3 lost 10.7-30.6 s on the build's first `mkdir`, and stalled for minutes in 2 of 3 runs on a slow link:
- **New nodes come from fresh blocks** (`State::take_new_nodes_from_fresh_blocks`, `lore-revision/src/state/node/slots.rs`). Reusing the revision's free slots had cost 48 remote slot loads per build, each a node block, its name table and its file-metadata block. The UnrealBuildTool compile fell from 28-29 s to 13-15 s.
- **Listings and the node blocks they hold have budgets of their own**: 256 MiB of listings, 1 GiB of blocks (`LISTING_BLOCK_BYTES`, `fs/fuse/listing_cache.rs`). A shared 256 MiB filled with 257 MB of blocks and evicted 43,000 listings, which UnrealBuildTool's makefile phase then rebuilt.
- **Caching a file walks a fragment list the local store already holds** (`push_local_list_children`, `lore-revision/src/immutable.rs`). A demand READ that stored a file's root list before the recording prefetch reached it made the prefetch skip the file. The 113 MB `ispc` then paged in one READ at a time behind the prefetch: 281-347 s, against 7 s when the prefetch won. This came from milestone 2; milestone 3 only lost the race more often. The same skip made FUSE readahead a no-op.

On a slow link every version misses the budget. On 2026-10-01 and 2026-10-02, at 2-6 MB/s, milestone 2 complete built in 245-277 s. A demand fetch waits behind the recording prefetch's backlog there (lore-hq8).

### Milestone 3 follow-ups

**Warm LOOKUP while the tree changes, at r1597**: the chain log, the tail work (T0-T3), CREATE and RENAME without a chain walk (lore-030) and the rename race fix (lore-pq1). Same tests and runner as below, three runs on 2026-10-04.

| Case | p50 | p99 | p99 of LOOKUPs that met no listing rebuild | LOOKUPs that met a rebuild |
|---|---|---|---|---|
| Saves only in other directories | 9.5-9.8 µs | 22-26 µs | 22-26 µs | 0% |
| Under concurrent writes | 24-64 µs | 124-165 µs | 118-160 µs | 0.2-0.3% |
| During a sync | 9.6-10.3 µs | 18-20 µs | 18-20 µs | 0% |
| Among creates in the directory | 126-131 µs | 228-239 µs | 216-227 µs | 0.3% |

- **The service answers fast in every case.** Its own LOOKUP is at p50 4 µs and p99 8-16 µs, with none at 131 µs or more. Every CREATE and RENAME takes 0 chain steps.
- **The rest is the kernel's directory lock.** Each CREATE, RENAME and UNLINK holds it exclusively while the service answers. A LOOKUP in that directory waits behind them: round trips of 131 µs or more number 15-68 under writes and 884-1,013 among creates.
- **Under concurrent writes, rebuilds are no longer the tail.** They fell from 1.3-2.2% of LOOKUPs to 0.2-0.3%, and p99 fell from 1.24-1.28 ms to 124-165 µs. The p50 varies between runs (13-69 µs at r1583, with only measurement added), so it tracks how the writers queue on the lock. The budget, p50 below 20 µs and p99 below 100 µs, is still not met end to end.
- **Among creates, the writers now finish 4-5 times more requests in the window.** CREATE's service time halved (p50 bucket 65.5 → 32.8 µs, p99 131 → 65.5 µs), so they hold the lock nearly back to back. The measured LOOKUP's p99 fell from 394-446 µs (r1583) to 228-239 µs, while its p50 rose from 9 µs to 126-131 µs. This case's budget is not asserted.

**Cold build against the remote**, at r1597 against milestone 3 (r1536), alternated as below (`sdwan0`, p95 33-52 MiB/s), on 2026-10-04: milestone 3 160.8, 166.7, 164.3 s (median 164.3); follow-ups 164.0, 163.5, 163.8 s (median 163.8, on par). The chain logs held 16.0 MB at each unmount, within the design's 17 MB estimate for logs at install size (15.9 MB at r1580).

**Rechecked at r1631**, after the symlink, reset, type-change, READDIRPLUS rollback and allocation fixes, on 2026-10-04. Same tests and runner, three runs each:
- **Warm LOOKUP:**
  - during a sync, p99 18-19 µs;
  - with saves elsewhere, p99 20-50 µs;
  - under concurrent writes, p50 11-58 µs and p99 117-156 µs;
  - among creates, p99 219-278 µs.

  Rebuilds meet 0.2-0.4% of LOOKUPs, and the service answers its own LOOKUPs at p99 8-16 µs, as at r1597.
- **Cold build,** against milestone 3, alternated: milestone 3 160.5, 164.5, 166.3 s (median 164.5); follow-ups 163.6, 163.7, 166.5 s (median 163.7, on par).
- **Chain logs:** 16.0 MB at each unmount.

**End to end on a repository**, at r1677 on 2026-10-12: a 440-file repository (6.98 MiB of sources and docs) on a local server, one virtual clone beside two ordinary ones. The steps:
- clone, and edits `status` sees unscanned;
- a sync bringing a remote change in and keeping the local edits;
- a sync refused on an edit the remote changes (exit 44, as on an ordinary clone), its dry run refused the same way, a forced dry run changing nothing, and a forced sync overwriting the edit and keeping the others;
- branch create, stage, commit and push from the mount, and switches both ways: one refused on an edit the switch changes, one carrying an edit it leaves;
- `reset` of a file, a merge of the branch into main pushed from the mount, and a sync restoring a file deleted locally that the remote changed.

After each step the virtual clone matched an ordinary one byte for byte, and each sync and switch verified as many changes.

**Commits and conflicts end to end**, at r1690 on 2026-10-12: the `lore-revision` sources, `lore-fuse` and the docs on a local server, one virtual clone beside an ordinary one, 46 of 46 checks. Beyond the steps above:
- a commit from the mount leaves status clean and its files gone from `write/`, still served;
- a commit of a link alone and of a layer refused, the revision and the staged edit left in place;
- a conflicting merge leaving markers, its resolve refused while they stay, then resolved, committed and pushed;
- a service restart remounting with a local edit kept.

**Allocations per request** (`lore-revision/tests/fuse_allocations.rs`, r1624, 300 runs under load):
- CREATE 4 (budget 5) and RENAME at most 2 (budget 2): the copy-up gates' shards are sized at open (lore-8kr).
- UNLINK by case (lore-udt):

  | Case | Budget | Measured maximum |
  |---|---|---|
  | Tracked file | 4 | 4 |
  | Local file | 6 | 5, or 6 in 10 of 300 runs |
  | Copied-up tracked file | 9 | 8, or 9 in 13 of 300 runs |

- Warm LOOKUP, GETATTR, READDIRPLUS and READ allocate nothing.

**Warm LOOKUP while the tree changes, with the chain log** (lore-dev), at r1580. The tests are the same as above (`lore-revision/tests/fuse_lookup_latency.rs`), run with `bench/m2/latency.sh`, three runs through `bench/m3/quiet-run.sh` on 2026-10-03.

| Case | p50 | p99 | p99 of LOOKUPs that met no listing rebuild | LOOKUPs that met a rebuild |
|---|---|---|---|---|
| Saves only in other directories | 9.7-10.0 µs | 34-90 µs | 34-90 µs | 0% |
| Under concurrent writes | 10.9-12.5 µs (82 µs in one run) | 1.24-1.28 ms | 180-217 µs | 1.3-2.2% |
| During a sync | 9.2-9.4 µs | 17-20 µs | 17-20 µs | 0% |
| Among creates in the directory | 9.1-10.8 µs | 0.45-2.2 ms | 0.39-0.48 ms | 0.7-2.0% |

- **During a sync, the budget now holds:** p99 fell from 1.16-1.30 ms to 17-20 µs, with no rebuilds; a swap no longer drops a listing.
- **Under concurrent writes, the tail is still rebuilds, plus a second cause.** A save logs four patches, so a 64-slot log overflows every 16 saves and the next LOOKUP rebuilds (about 1.25 ms). LOOKUPs that met no rebuild still have a p99 of 180-217 µs.
- **Among creates, the tail without rebuilds is 0.39-0.48 ms.** At r1580 each CREATE's duplicate-name check walked the directory's whole chain (lore-030), about 2,000 steps in a 2,000-entry directory.

**Cold build against the remote**, at r1580 against milestone 3 (r1536), alternated on one link (`sdwan0`, p95 37-52 MiB/s), three runs each through the quiet runner, clone included, on 2026-10-03: milestone 3 160.6, 164.9, 164.3 s (median 164.3); follow-ups 165.8, 165.1, 165.4 s (median 165.4, +0.7%, inside milestone 3's spread). The cache-get cap is at its default, 512.

### State node operations: measurements

**Instructions per operation**, against r1858, counted one thread at a time at r2054 on
2026-10-31: the workloads of `lore-revision/examples/state_bench` and the C API's verbs in
`lore/examples/revision_tree_bench`.
- Every workload is within +0.5% of r1858 or below it, most 10% to 95% below, but two with a
  budget of 2×, accepted: a directory mark climbing eight settled levels (`served-mark-climb`),
  1.42×, and the first unlink below a deep settled chain (`served-unlink-settled-deep`), at most
  1.08×. A served change needs every directory above it dirty, so the first one below settled
  levels marks each of them.
- The C API: add −23.5%, modify −44.7%, delete of an added file −58.0%, delete of a tracked file
  −63.8%, delete, add again and commit −47.0%, commit −0.25%; move within +0.5%.

**Wall clock through the mount**, the ignored benchmarks of
`lore-revision/tests/unit/fs/fuse/write_mount.rs`, against r1858, 32 alternating rounds per
profile on a second machine, pinned to one thread per core, on 2026-10-31:

| Bench | release | release-lto |
|---|---|---|
| Commit of 400 written files | −3.0% | −2.6% |
| The same, from reconciliation on | −9.9% | −7.8% |
| Status, stage, and local read and write latency | within ±1% | within ±1% |
| Sync of 1000 changes | −1.3% | −1.2% |
| Commit of 1000 empty directories, siblings / a moved tree, with the status after it | +63% / +143% | +72% / +144% |
| Commit of 1000 directories holding a file each | +1.9% | +2.8% |

- **Empty directories, accepted.** A commit now settles each empty directory it stores and
  removes its host directory, which every later status repays: 0.3 ms against 2.5 ms.
- **Directories holding files, accepted.** An earlier build of this work, measured at +0.6% and
  +1.6% on the i7-13800H named under Measured performance, shows the same rise on the second
  machine, so the rest is the machine's.
- **The commit's walk of dirty nodes** now descends a staged move's or delete's subtree, so the
  changes below it that were not staged are no longer lost: 0.52 ms per 10,201 nodes. Folding it
  into the commit's prune walk would remove that pass.

## Remaining work and things to test

Open follow-ups from milestone 3:

- **Ignore rules on a virtual clone: dirty adds.** A command reads a virtual clone's ignore
  file from the provider, from `write/` or the store, never through the service's own mount
  (`load_held_filter`, `lore-revision/src/repository.rs`; `read_held_file`, `filesystem.rs`), and
  the provider diff answers whether lore's tree tracks a path from that tree, its from side, never
  from the served State, whose node IDs part from its base's after a swap
  (`DiffFlags::FilesystemTo`, `state/diff.rs`). Left: an add only marked dirty that a rule excludes
  is left out of the report but stays marked in lore's tree, where an ordinary clone's scan drops it
  (`state/os_diff.rs`, with `stage::drops_cleanly`); queue it for discard in the diff's settle.
- **A narrowing over a local copy of revision content.** A view change checks what each view
  shows, but applies only the revision's own changes, so a local copy still holding what the
  revision holds, at a path the new view drops, passes the check and stays listed as local
  content outside the view, where an ordinary sync deletes it (`hold::shown_changes`,
  `swap::swap_through`). A directory whose only local change is a deletion below it stays listed
  the same way (`listing::holds_local`).
- **Directories a re-inclusion may reach.** A view whose re-inclusion opens with a wildcard,
  such as `/*` with `!*/Saved/Config/`, leaves every excluded directory one the rule may reach
  below, so the mount lists it, empty where it holds nothing in view, where an ordinary clone
  creates no directory it finds nothing in (`child_filter`, `view.rs`). Left: hide such a
  directory until something below it is in view, which takes a walk below it per listing.
- **Links.** Adding a link to a virtual clone is refused (`lore-revision/src/link/add.rs`): the
  provider serves no link, and a link node in the State failed the node block's child assertion,
  which aborts the service.
- **A link a swap adds.** A swap copies a node the target revision adds without its `Link`
  flag (`hold::copy_of`), so a repository link the target adds is served as an empty directory,
  where a mount refuses a revision holding one (`Listing::build_with`, `listing.rs`). Left: refuse a
  sync or switch whose target adds a link, in `hold::check`.
- **Detached slots of the served State.** A request dropped or refused part way can leave a slot
  detached or taken and never published, and an unlinked node's slot is never reused within a
  mount. The State node module frees such slots with a sweep at quiescent points
  (`ServedNodes::node_sweep_detached`, which takes the provider's test of whether the kernel still
  references a node), but the provider never calls it, so they stay allocated until the next
  mount rebuilds the State. Left: a sweep where the provider has paused its requests, settles and
  swaps, keeping the nodes the kernel still references, and a cached listing dropped or rebuilt
  after it.
- **Warm LOOKUP waits on the kernel's directory lock** (lore-a0p). The chain log (lore-dev, closed)
  leaves rebuilds at 0.2-0.3% of LOOKUPs under concurrent writes, and the service answers its own
  LOOKUPs at p99 8-16 µs (r1597, Milestone 3 follow-ups above). End to end, the tail under
  concurrent writes (p50 24-64 µs, p99 124-165 µs) and among creates (p50 126-131 µs) is LOOKUPs
  queued on the directory's `i_rwsem` behind CREATE, RENAME and UNLINK, which hold it for their
  whole round trip. Left: shorter service times for those requests, lookups answered from the
  kernel's cache while writes run, and a real build's share of queued LOOKUPs to size the work.
- **What a hold's check compares.** It compares content only for a local edit of a tracked
  file at the path a change replaces or removes, or below a directory the change removes: a
  file added locally where the target adds one, or a link, is refused whatever it holds, and so
  is a directory the target replaces with a file holding a local edit of the content it
  replaces, where an ordinary clone compares each file (`hold::check`). A refused path is named without the sizes an ordinary clone gives. A sync
  forwarding local changes reports every change the view shows, those it leaves local included,
  where an ordinary clone reports only what it writes (`hold::report`). A sync resetting local
  changes (`--reset`) is refused on a virtual clone, as is one carrying layers or restricted to a
  dependency set (`lore-revision/src/revision/sync.rs`).
- **A hold decodes the changed blocks twice.** A sync or branch switch diffs a clean State of the
  served revision against the target (`hold::changes`): the served State carries the local
  changes in place, so it cannot stand for the revision. Each block the diff visits is decoded a
  second time beside the one the served State holds, and stays resident until the hold ends, as
  the changes refer to it. Sharing the blocks the served State has not modified, copying one
  before its first edit, would remove that; due before the FUSE code goes up for merge.
- **Chain logs and the block budget** (lore-zep). The logs hold 16.0 MB after a cold build, uncharged to the
  block budget; charge them if they come to matter.
- **READDIRPLUS across a swap** (lore-czh). A stream read across any swap lists plain, even where the swap
  left its directory alone (the swap count in `stream_current` and `push_dirents_plus`).
- **A merge failing part-way** (lore-pq2). An error part-way through a merge's apply on a virtual
  clone can leave changes applied with nothing staged. A symlink at a written path no longer
  triggers it (lore-ubp refuses up front); the general case is open.
- **Instance writes carry no address** (lore-ew0). Plan decision 9 has `write_node` and
  `set_file_to_immutable_store_contents` give the node the content's address, `LocalContent` and
  no `Dirty`. They write as a user does instead, zero address and `Dirty`: a WRITE to a file
  already local never touches its node (`write_local_file`), so a later user write would leave
  that address standing over other bytes and status would miss the edit. Status therefore hashes
  each file a merge wrote until a commit stores it, stored content is copied twice (staged, then
  written), and a reset that restores a file deleted locally leaves a local copy equal to the
  revision. `make_executable` on a tracked file copies it up too: a mode on a store-served node
  alone would not survive a remount, which rebuilds from the revision and `write/`.
- **Spurious `ENOENT` below a directory being renamed** (lore-96e). A RENAME builds its source
  host path unwatched, so a RENAME out of a directory while that directory is being renamed can
  answer `ENOENT`; the State is unchanged. Other host operations by path below such a
  directory behave the same way. A third rename of one directory landing between another's
  respell and its gather can leave an entry under the old host path (`gather_host_dir`).
- **A file arriving with the recorded time and size** (lore-2yv; all clones). Status and stage
  trust a recorded modification time when the size matches, in milliseconds. A different file
  renamed over the path with the same size and millisecond, or one written by `cp -p`, `tar x`
  or `rsync -t`, is never read. On a virtual clone the provider handles the rename case in RENAME;
  ordinary clones still miss it.
- **A stage that only discards in nested repositories** (lore-q53; all clones). `stage()` saves
  the staged State only where the main repository changed, so a stage whose only effect is
  discarding entries queued for nested repositories loses those discards.
- **Residue that equals the revision** (lore-5xd). A local file whose content equals the current
  revision, as a refused flip or a rename back leaves, stays local until a later commit stores
  it, and every status hashes it again. A sync leaves a local file whose content equals the
  target's the same way. A marking diff that finds such a file could flip it as a commit does.
- **Desktop trash** (lore-9fr). See "Desktop trash" under Writes.
- **Prefetch and demand fetches on a slow link** (lore-58g). On a slow link a recording's
  prefetch fills it for the whole build, and every demand fetch queues behind it; milestone 2 is
  affected too. Cache gets in flight are capped on each connection (`CACHE_GETS`, 512; see
  the knobs, lore-hq8), with priority gets exempt; 512 has no measured cost on a fast link. The
  slow-link effect, a demand fetch within a few round trips at about 5 MB/s, is unmeasured:
  throttling the link needs root.
  Readahead fetches alongside the prefetch; a cache fetch joins a read or fetch of the same
  content in flight, so the two share one get of a fragment.

Host steps around State operations. A served request's State operation dropped or refused at any
point leaves the State whole, at worst leaking a slot or leaving a directory dirty; the host steps
around it cannot yet, so a cancelled commit or requests racing can leave `write/` or the deletion
list disagreeing with the State, and the next mount then loses or brings back a change:

- **The deletion list across a remount and swaps.** A commit cancelled between a flip's host
  removal and the deletion list's rewrite leaves a listed path with no `write/` entry. A mount
  keeps a listed directory that has a same-kind host entry with the revision's children
  (`apply_deletions`). An RMDIR of an added directory during a swap goes unrecorded where it runs
  between the swap's apply and its settle. A removal appended after a swap read the list names a
  path the swap placed again. A commit's flip below a directory being renamed lets the host move
  carry the directory without the file.
- **Renames over directories.** An entry made in a directory rename's source while its rollback
  runs lands in, or fails over, the moved host directory; copy-ups take no claim, and an
  instance's `rename_at` no VFS lock. A rename over a directory can replace an attach that lands
  between its emptiness check and its State step.
- **RMDIR's host steps.** A create losing to an RMDIR or a rename over its directory removes its
  entry and the host directory it made again, unless the State or a claim holds the place; the
  races left are a rename carrying the entry away first, a third request reusing the entry's
  path, and a worker cleanup whose chain block left memory. An RMDIR putting its host directory
  back after a refused detach can re-make it for a directory another remover detached.
- **Host-directory removal and the settle log.** A settle's deferred removal re-makes a directory
  a create made again meanwhile, with the default mode. `SettleLog::record` assumes one writer,
  and a content restore records without `swap_plan`; whether the instance's write lock keeps it
  apart from a commit's reconciliation is unverified. A swap removes a cleared directory's host
  directory without recording it, so a rename that resolved it answers `ENOENT`. A swap that
  restores a directory level leaves the kernel's negative dentry for it.
- **Widenings to keep or narrow.** After a dropped commit, every rename away of an added file
  appends a deletion line; a commit's pre-record lists a handed directory the user removed before
  the commit; a directory rename refused at its State step takes the host rollback.

State node operations:

- **Stress suite coverage.** The deterministic scenarios compose the operations as the adds, the
  scan's directory chains, the stage's claims, and the moves and respellings do. Not built: the
  compositions of the discards, unstage's clears, the commit and its settle, the swap's restore,
  served writers outside a swap, a RENAME over a directory, the writers before serving and the C
  API's batches; generated pairs of operations over placements, with their coverage; and chaos
  that cancels, evicts blocks, injects faults or rotates served States.
- **A save beside slot allocations.** A slot allocation whose read of the unused chain's head
  spans a block's retirement, a slot freed in it and a save threading it can thread that block
  twice. Latent, as no save runs beside writers: the threaded test runs its saves between bursts
  of writers (`saves_between_bursts_of_creates_and_discards_keep_the_unused_chain_and_every_node`,
  `lore-revision/tests/state_scenarios/threads.rs`). Each allocation checking the head under its
  block's lock would close it, at a cost on every allocation.
- **Product bugs outside the module**, found during the State node work and left as they are on
  main: the swap recovery map's replay of a path an earlier failed apply changed (a child the user
  deleted can come back); merge flags (a side-resolved merge reported unresolved, a take-theirs of
  an incoming deletion staged as a keep); a C API modify replacing a staged move; a repository
  linked at two mount paths, whose two stage tasks may add one name; `lore stage move` onto a held
  name, which leaves two children under one name until the commit (lore-bzze), and
  `lore dirty move` onto one, which, from reading the code, leaves the holder live beside the
  moved node (lore-ape2); and lore-storage's partition-keyed in-flight writes.
- **Performance follow-ups**, optional, each measured against the gate: one guard across a
  climb's same-block levels, the commit's dirty walk folded into its prune walk, and a stage
  claim's walk of its earlier adds (N²/2 visits for N new files).

Writes and commits:

- **Committed times after FORGET** (lore-ew4). A flipped inode keeps its modification time while the kernel
  holds it; looked up again after a FORGET it reports the base time, as a tracked file does.
- **Disk held by flipped descriptors** (lore-cmt). A flipped file's cached descriptor keeps its unlinked
  host file's blocks until FORGET, a later copy-up, or descriptor-cache eviction.
- **Transient omission after a no-inode flip** (lore-6uh). Between a batch flip and the put-back of a file
  something reached meanwhile, the node stands clean, and a status running then misses the file
  until the next status.
- **Symlinks are not versioned** (lore-ubp). As on an ordinary clone, whose walk skips them
  (`file_list_item`, `fs/os/mod.rs`), status, stage and commit take a symlink as nothing there: one
  made through the mount, alone or in a directory made with it, stays local under `write/` and
  never shows as a change, and a tracked file a symlink replaced reads as deleted
  (`versioned_on_a_working_tree`, `state/diff.rs`). `lore stage` naming one skips it, reporting
  the path ignored (`PathIgnore`) as status does for a path holding nothing, and stages the other
  paths named with it; a path below one fails as a path below nothing does
  (`stage_filesystem_path`). An ordinary clone's stage follows the link and stages the target's
  bytes. A tracked file a symlink replaced, named in `lore stage`, stages its deletion. A file
  staged and then replaced by a symlink fails the commit with `FileNotFound`, as one deleted after
  staging does.
- **Writes meet a symlink** (lore-ubp). Only versioning skips one. A sync, merge or cherry-pick
  that would write where a symlink stands refuses with `LocalModifications` before applying
  anything, whatever the symlink names, as for a local edit: a tracked file or directory one
  replaced, or an untracked one at a path the target adds (`hold::check`, `fs/hold.rs`, for a
  sync; `verify_filesystem`, `fs/realize.rs`, for a merge or cherry-pick). A merge or
  cherry-pick refuses whatever `forward_changes` says, as it writes every change it verified. A
  sync with `--forward-changes` keeps the symlink and applies the rest of the target. A forced
  sync skips the check, and the apply keeps a symlink standing where the target changes a
  tracked file (`remove_node`, `fs/hold.rs`;
  `overwrite_local_file`, `fs/fuse/write.rs`): the target's change there is not applied, the rest
  is, and status then lists the tracked file deleted. `lore reset <path>` is the one command that
  replaces a symlink with the file (lore-as0).
- **Forced sync over local directories** (lore-5i2). Below a directory the target removes, `--force`
  removes the files the served revision holds and keeps files added locally, with their host
  files and the directories above them, as an ordinary clone does. A directory the target turns
  into a file keeps the files added below it, and the target's file is not placed there.
- **Deletions lost to a crash** (lore-qgr). The deletion list is appended without `fsync` (plan decision 2),
  so a crash may lose the last deletions, and those paths reappear at the next mount.
- **Directory renames and hard links** (lore-1j0; allocations lore-2xj). A tracked or merged directory rename is `EXDEV`, but for
  a change of case alone; LINK is `EPERM`. A create below a rename still under way when its wait
  runs out (`RENAME_WAITS`) still has its entry moved: by its own settle where the rename has
  respelled the directory, else by the rename's gather after the respell. A directory rename
  allocates its old host path, to gather, and a raced entry its path; directory renames sit
  outside the file-rename allocation budget.
- **Directory times** (lore-cjx). The time a write stamps on a tracked directory lives in the inode table; a
  remount reports the base or swap time again.
- **Copy-up deadline** (lore-k08). A deferred copy-up fetches remote content with no deadline, unlike a cold
  READ; an INTERRUPT ends the wait.
- **Writeback cache** (lore-any) is off; measure it under a build (milestone 2 plan decision 6).
- **Host file budget** (lore-j4z). `CACHED_FILES` is fixed rather than derived from `RLIMIT_NOFILE`.

Reads, listings and prefetch:

- **Serial start of a cold tree** (lore-bsq). Without a recording, the first seconds of parallel cold access
  wait on one chain of listings, `/` → `Engine` → `Engine/Source` → `Engine/Source/Runtime` (12,
  10, 7 and 41 node blocks): 10-13 s during which every reader waits on the same builds. A
  recording's block indices remove the wait for what it recorded; the `blocks` prefetch reaches
  the rest in index order, not in the order readers need it.
- **Duplicate name-table fetches** (lore-yu3). Two directories whose sibling chains cross one
  block may both fetch its name table: 79 of 1,254 name-table fetches in the parallel run above
  (r1348). `NodeBlock::deserialize_nametable` (`lore-revision/src/state/block.rs`) has no single
  flight of its own, but the storage read single-flight (`lore-storage/src/read_flight.rs`, since
  r1319) sits on its path, so why they repeated is not known.
- **Remote fetch throughput** (lore-acu). Over the 140 ms path to `lore.internal.epicgames.net` one storage
  connection delivers 2-4 MiB/s, 16 deliver 25-29 MiB/s and 32 deliver 38-40 MiB/s (2026-09-25).
  Whether the per-connection cap is the server's congestion control or its send window is not
  known from the client. A command opens one connection unless `--max-connections` or
  `LORE_MAX_CONNECTIONS` asks for more, at most 10.
- **Cold listing cost** (lore-5hr). A listing whose blocks the prefetch has not reached costs one round trip
  per uncached node block on the directory's sibling chain, plus one: 5.8 s for
  `Engine/Source/Runtime` (39 uncached blocks). A per-directory index of child blocks in the tree
  format would bound it at two round trips per level whatever the prefetch has done; as a
  format change it needs a LEP first.
- **First listing after a mount or swap** (lore-ktz) reads the whole node block address list first: 0.9-1.2 s
  for Fortnite's 1.4 MB list. Reading only the sub-fragment holding the wanted block (the
  `TODO(mjansson)` in `read_node_block_addresses`, `lore-revision/src/state.rs`) would cost one
  more round trip per sub-fragment touched instead.
- **The default is not measured** (lore-egj). The measurements above ran S6, which read uncompressed leaves in
  place; the default keeps them decoded (`Decoded(4)`), as it does compressed ones.
- **Sequential prefetch default** (lore-6w5). Sequential prefetch is off. It doubles single-stream cold reads
  and slows 16 concurrent streams by 28–39%. Capping concurrent prefetching streams is an option.
- **READDIR-first policy** (lore-cc9). Plain-first makes cold `find` 2.7× faster and cold `ls -l` 3.6× slower.
  It is off. An adaptive policy is an option.
- **Prefetch memory** (lore-bee). A tree walk keeps every block it loaded, and its task, until the walk ends
  (`block_discover_task`, `repository/clone.rs`); `all` held 26,019 blocks. `blocks` holds
  the payloads of the 512 blocks in flight.
- **File-metadata blocks** (lore-duo). `blocks` fetches all of them (66 of 1076 MiB in the prefetch table),
  by milestone 2 plan decision 2, though the provider reads none: new nodes take fresh blocks.
- **Prefetch contention** (lore-bd9). While the recording's prefetch fills the link (30-54 MiB/s), the
  build's own misses share it: UnrealBuildTool compiled in 14.3 s with READs fetching at
  priority (18.2 s at normal priority), against 12.6-13.2 s on a real clone. Listing builds for
  kernel requests and for the prefetch's paths share the priority streams.
- **Repository links** (lore-6ib). A listing that holds a Lore link node (a linked repository)
  fails with `NotSupported` (`Listing::build_with`, `listing.rs`). READLINK answers local symlinks
  only.
- **Kernel readahead cap.** An unprivileged mount gets 128 KiB kernel readahead whatever
  `max_readahead` negotiates. Kernel behaviour: only root can raise the mount's `read_ahead_kb`,
  so it is not tracked.
- **SIGINT on buffered remote reads.** A buffered read of a remote-only file does not end on
  SIGINT; this is kernel behaviour. The service handles every INTERRUPT it receives, deferred
  READs included (`src/dispatch.rs`, the `Request::Interrupt` arm of `dispatch_request`).
- **Offline EIO timing** (lore-1ju). Offline fetches fail without waiting on the deadline; measure the actual
  time to `EIO` through session resolution and retries.
- **Uncalled cells** (lore-dam). Rerun W4 warm and `find` warm under a quiet, memory-rich run.

Service, stores and platform:

- **Served lock** (lore-kve). Every request loads what is served under a `parking_lot::RwLock` read lock
  (`ServedCell::load`, `fs/fuse/served.rs`). An atomically swapped pointer would make the
  load lock-free.
- **Test sandboxes and the global registry** (lore-rtz). The unit binary and `fuse_mount` point
  `LORE_GLOBAL_PATH` at a temporary directory held in a static, which is never removed, and
  `lore-integration-tests` makes one with no cleanup. Test binaries that set no
  `LORE_GLOBAL_PATH` may still register `/tmp` shared stores in the user's
  `stores/registry.toml`; service start logs and skips each one it cannot open or list.
- **Lock-free store index** (lore-abz). A local read takes the store's bucket and pack locks; an
  atomically swapped immutable index would remove them.
- **Pack layout and cold-read amplification** (lore-3kp). Pack files group payloads by hash, so
  one ZFS record mixes fragments of many files and cold reads pull extra data from disk.
- **FUSE over io_uring** (lore-3a9). Not implemented. The transport boundary (`src/transport/mod.rs`) is where
  it goes; the kernel's `fuse.enable_uring` parameter is off by default.
- **CI** (lore-57l). No workflow installs fuse3 or checks `/dev/fuse`; mount tests skip through
  `skip_without_fuse!` when FUSE is unusable, so a green run does not show they ran.
- **Debug unmount time** (lore-70p). A plain unmount in a debug build waits at least 100 ms
  (`DRAIN_SETTLE`) and up to 2 s (`DRAIN_WAIT`) for FORGETs before unmounting.
- **Automatic open-mode fallback** (lore-kqn). The service mount always answers OPEN with `ENOSYS`; no code
  selects `KeepCache` for a kernel that lacks `NO_OPEN_SUPPORT`.
- **Expire-only invalidation** (lore-3fw). `HAS_EXPIRE_ONLY` is negotiated, but swaps send `inval_entry` with
  `expire_only = false` (`inval_entry`, `swap.rs`).
- No `TODO`, `FIXME` or `follow-up` markers exist in the FUSE sources.
