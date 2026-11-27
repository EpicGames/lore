---
type: breaking
---

- `lore-client`: `--no-background-flush` (C API: `no_background_flush` on `lore_global_args_t`) leaves the local
  immutable store's background index writes until the command ends: the delayed flushes of its changes, and the writes
  of eviction passes, which still run and evict in memory, so a long clone keeps its store within its capacity without
  rewriting index files while it writes the working tree. The command's own flushes write as before. It writes what it
  held back before it completes, whether or not it fails, and a command that otherwise succeeds fails if that write
  fails. Independent of `--no-gc`. A storage handle opened with the option holds the writes back until
  `lore_storage_close`. In a process that serves several calls, such as the Lore service or a C API program with open
  storage handles, the background writes of every other call on the same store wait as well until the call or handle
  with the option ends. If the process stops meanwhile, the index loses the changes held back: content from the remote
  is fetched again, and content written locally stays in the packfiles unindexed until compaction reclaims it.
- `lore`: the field takes padding in `lore_global_args_t`, whose size and other fields are unchanged. A call relayed
  to the Lore service carries one more variable, so a client and a running service from either side of this release
  refuse each other's calls. Stop a running Lore service with `lore service stop` after upgrading.
