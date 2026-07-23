# Lore CLI command reference

The `lore` command-line client drives every local and remote Lore operation: creating and cloning repositories, staging and committing revisions, branching, merging, and syncing with a Lore Server. This page catalogs every `lore` command and subcommand, with its arguments and flags.

This page documents the command surface only. For a guided first run, see the [Quickstart](../tutorials/quickstart.md); to install the client, see [Install the Lore CLI](../how-to/install-lore-cli.md).

This page is generated from `lore --markdown-help` (CLI `0.8.2-nightly+31`). Everything below the marker is generated — change the CLI, not this section. To regenerate in place (preserving this header), run from the repository root:

```bash
{ sed '/^<!-- BEGIN generated/q' docs/reference/lore-cli-commands.md; lore --markdown-help | tail -n +4; } > docs/reference/.cli.tmp && mv docs/reference/.cli.tmp docs/reference/lore-cli-commands.md
```

<!-- BEGIN generated: lore --markdown-help -->

**Command Overview:**

* [`lore`↴](#lore)
* [`lore repository`↴](#lore-repository)
* [`lore repository status`↴](#lore-repository-status)
* [`lore repository info`↴](#lore-repository-info)
* [`lore repository list`↴](#lore-repository-list)
* [`lore repository create`↴](#lore-repository-create)
* [`lore repository clone`↴](#lore-repository-clone)
* [`lore repository delete`↴](#lore-repository-delete)
* [`lore repository verify`↴](#lore-repository-verify)
* [`lore repository verify state`↴](#lore-repository-verify-state)
* [`lore repository verify fragment`↴](#lore-repository-verify-fragment)
* [`lore repository dump`↴](#lore-repository-dump)
* [`lore repository gc`↴](#lore-repository-gc)
* [`lore repository store`↴](#lore-repository-store)
* [`lore repository store immutable`↴](#lore-repository-store-immutable)
* [`lore repository store immutable query`↴](#lore-repository-store-immutable-query)
* [`lore repository metadata`↴](#lore-repository-metadata)
* [`lore repository metadata get`↴](#lore-repository-metadata-get)
* [`lore repository metadata set`↴](#lore-repository-metadata-set)
* [`lore repository metadata clear`↴](#lore-repository-metadata-clear)
* [`lore repository instance`↴](#lore-repository-instance)
* [`lore repository instance list`↴](#lore-repository-instance-list)
* [`lore repository instance prune`↴](#lore-repository-instance-prune)
* [`lore repository config`↴](#lore-repository-config)
* [`lore repository config get`↴](#lore-repository-config-get)
* [`lore repository update-path`↴](#lore-repository-update-path)
* [`lore branch`↴](#lore-branch)
* [`lore branch list`↴](#lore-branch-list)
* [`lore branch info`↴](#lore-branch-info)
* [`lore branch create`↴](#lore-branch-create)
* [`lore branch switch`↴](#lore-branch-switch)
* [`lore branch push`↴](#lore-branch-push)
* [`lore branch merge`↴](#lore-branch-merge)
* [`lore branch merge unresolve`↴](#lore-branch-merge-unresolve)
* [`lore branch merge into`↴](#lore-branch-merge-into)
* [`lore branch merge start`↴](#lore-branch-merge-start)
* [`lore branch merge restart`↴](#lore-branch-merge-restart)
* [`lore branch merge resolve`↴](#lore-branch-merge-resolve)
* [`lore branch merge resolve mine`↴](#lore-branch-merge-resolve-mine)
* [`lore branch merge resolve theirs`↴](#lore-branch-merge-resolve-theirs)
* [`lore branch merge abort`↴](#lore-branch-merge-abort)
* [`lore branch diff`↴](#lore-branch-diff)
* [`lore branch archive`↴](#lore-branch-archive)
* [`lore branch reset`↴](#lore-branch-reset)
* [`lore branch protect`↴](#lore-branch-protect)
* [`lore branch unprotect`↴](#lore-branch-unprotect)
* [`lore branch latest`↴](#lore-branch-latest)
* [`lore branch latest list`↴](#lore-branch-latest-list)
* [`lore branch metadata`↴](#lore-branch-metadata)
* [`lore branch metadata get`↴](#lore-branch-metadata-get)
* [`lore branch metadata set`↴](#lore-branch-metadata-set)
* [`lore branch metadata clear`↴](#lore-branch-metadata-clear)
* [`lore revision`↴](#lore-revision)
* [`lore revision history`↴](#lore-revision-history)
* [`lore revision info`↴](#lore-revision-info)
* [`lore revision commit`↴](#lore-revision-commit)
* [`lore revision amend`↴](#lore-revision-amend)
* [`lore revision sync`↴](#lore-revision-sync)
* [`lore revision bisect`↴](#lore-revision-bisect)
* [`lore revision diff`↴](#lore-revision-diff)
* [`lore revision find`↴](#lore-revision-find)
* [`lore revision find metadata`↴](#lore-revision-find-metadata)
* [`lore revision find number`↴](#lore-revision-find-number)
* [`lore revision restore`↴](#lore-revision-restore)
* [`lore revision cherry-pick`↴](#lore-revision-cherry-pick)
* [`lore revision cherry-pick unresolve`↴](#lore-revision-cherry-pick-unresolve)
* [`lore revision cherry-pick restart`↴](#lore-revision-cherry-pick-restart)
* [`lore revision cherry-pick resolve`↴](#lore-revision-cherry-pick-resolve)
* [`lore revision cherry-pick resolve mine`↴](#lore-revision-cherry-pick-resolve-mine)
* [`lore revision cherry-pick resolve theirs`↴](#lore-revision-cherry-pick-resolve-theirs)
* [`lore revision cherry-pick abort`↴](#lore-revision-cherry-pick-abort)
* [`lore revision revert`↴](#lore-revision-revert)
* [`lore revision revert unresolve`↴](#lore-revision-revert-unresolve)
* [`lore revision revert restart`↴](#lore-revision-revert-restart)
* [`lore revision revert resolve`↴](#lore-revision-revert-resolve)
* [`lore revision revert resolve mine`↴](#lore-revision-revert-resolve-mine)
* [`lore revision revert resolve theirs`↴](#lore-revision-revert-resolve-theirs)
* [`lore revision revert abort`↴](#lore-revision-revert-abort)
* [`lore revision metadata`↴](#lore-revision-metadata)
* [`lore revision metadata clear`↴](#lore-revision-metadata-clear)
* [`lore revision metadata get`↴](#lore-revision-metadata-get)
* [`lore revision metadata set`↴](#lore-revision-metadata-set)
* [`lore file`↴](#lore-file)
* [`lore file info`↴](#lore-file-info)
* [`lore file metadata`↴](#lore-file-metadata)
* [`lore file metadata clear`↴](#lore-file-metadata-clear)
* [`lore file metadata get`↴](#lore-file-metadata-get)
* [`lore file metadata set`↴](#lore-file-metadata-set)
* [`lore file dependency`↴](#lore-file-dependency)
* [`lore file dependency add`↴](#lore-file-dependency-add)
* [`lore file dependency remove`↴](#lore-file-dependency-remove)
* [`lore file dependency list`↴](#lore-file-dependency-list)
* [`lore file stage`↴](#lore-file-stage)
* [`lore file stage move`↴](#lore-file-stage-move)
* [`lore file stage merge`↴](#lore-file-stage-merge)
* [`lore file dirty`↴](#lore-file-dirty)
* [`lore file dirty move`↴](#lore-file-dirty-move)
* [`lore file dirty copy`↴](#lore-file-dirty-copy)
* [`lore file unstage`↴](#lore-file-unstage)
* [`lore file reset`↴](#lore-file-reset)
* [`lore file obliterate`↴](#lore-file-obliterate)
* [`lore file history`↴](#lore-file-history)
* [`lore file diff`↴](#lore-file-diff)
* [`lore file write`↴](#lore-file-write)
* [`lore file hash`↴](#lore-file-hash)
* [`lore auth`↴](#lore-auth)
* [`lore auth login`↴](#lore-auth-login)
* [`lore auth info`↴](#lore-auth-info)
* [`lore auth list`↴](#lore-auth-list)
* [`lore auth logout`↴](#lore-auth-logout)
* [`lore auth clear`↴](#lore-auth-clear)
* [`lore layer`↴](#lore-layer)
* [`lore layer add`↴](#lore-layer-add)
* [`lore layer remove`↴](#lore-layer-remove)
* [`lore layer list`↴](#lore-layer-list)
* [`lore logfile`↴](#lore-logfile)
* [`lore logfile info`↴](#lore-logfile-info)
* [`lore login`↴](#lore-login)
* [`lore link`↴](#lore-link)
* [`lore link add`↴](#lore-link-add)
* [`lore link remove`↴](#lore-link-remove)
* [`lore link update`↴](#lore-link-update)
* [`lore link list`↴](#lore-link-list)
* [`lore status`↴](#lore-status)
* [`lore clone`↴](#lore-clone)
* [`lore stage`↴](#lore-stage)
* [`lore stage move`↴](#lore-stage-move)
* [`lore stage merge`↴](#lore-stage-merge)
* [`lore dirty`↴](#lore-dirty)
* [`lore dirty move`↴](#lore-dirty-move)
* [`lore dirty copy`↴](#lore-dirty-copy)
* [`lore unstage`↴](#lore-unstage)
* [`lore reset`↴](#lore-reset)
* [`lore diff`↴](#lore-diff)
* [`lore history`↴](#lore-history)
* [`lore commit`↴](#lore-commit)
* [`lore sync`↴](#lore-sync)
* [`lore push`↴](#lore-push)
* [`lore lock`↴](#lore-lock)
* [`lore lock acquire`↴](#lore-lock-acquire)
* [`lore lock status`↴](#lore-lock-status)
* [`lore lock query`↴](#lore-lock-query)
* [`lore lock release`↴](#lore-lock-release)
* [`lore service`↴](#lore-service)
* [`lore service run`↴](#lore-service-run)
* [`lore service start`↴](#lore-service-start)
* [`lore service stop`↴](#lore-service-stop)
* [`lore service set-use-automatically`↴](#lore-service-set-use-automatically)
* [`lore notification`↴](#lore-notification)
* [`lore notification subscribe`↴](#lore-notification-subscribe)
* [`lore completions`↴](#lore-completions)
* [`lore shared-store`↴](#lore-shared-store)
* [`lore shared-store create`↴](#lore-shared-store-create)
* [`lore shared-store info`↴](#lore-shared-store-info)
* [`lore shared-store set-use-automatically`↴](#lore-shared-store-set-use-automatically)

## `lore`

**Usage:** `lore [OPTIONS] [COMMAND]`

###### **Subcommands:**

* `repository` — Repository commands
* `branch` — Branch commands
* `revision` — Revision commands
* `file` — File commands
* `auth` — Authentication commands
* `layer` — Layer commands
* `logfile` — Logfile commands
* `login` — Authenticate the CLI
* `link` — Link commands
* `status` — Show current repository status
* `clone` — Clone a remote repository into the given path
* `stage` — Stage changes for commit
* `dirty` — Mark files as dirty so they show up in `lore status` and get picked up by `lore stage` (no content is read or staged)
* `unstage` — Unstage changes to a file or directory
* `reset` — Reset changes to a file or directory
* `diff` — Show differences between two revisions of a file
* `history` — List revisions of a repository
* `commit` — Commit the staged revision
* `sync` — Synchronize to a repository state
* `push` — Push commits to remote
* `lock` — Lock file
* `service` — Manage the repository in a service process
* `notification` — Notifications
* `completions` — Generate terminal autocompletions
* `shared-store` — Manage the shared store

###### **Options:**

* `--repository <path>` — Use given path as repository path
* `--log-level <level>` — Set the logging level
* `-d`, `--debug` — Enable debug output
* `-f`, `--force` — Force the operation if possible
* `--dry-run` — Dry run mode, only report what would have been changed and perform no changes to local file system
* `-P`, `--no-pager` — Disable pagination
* `--offline` — Force offline mode
* `--remote` — Use remote data
* `--local` — Use local data
* `--identity <IDENTITY>` — Use given identity
* `--max-connections <MAX_CONNECTIONS>` — Set maximum number of parallel connections
* `--file-count-limit <count>` — Set maximum number of parallel files opened
* `--file-size-limit <size>` — Set maximum total size in bytes of parallel files opened
* `--compress-limit <count>` — Set maximum number of parallel compress operations
* `--search-limit <SEARCH_LIMIT>` — Set maximum number of revisions to search when matching or finding revisions
* `--search-nearest` — Set to search for nearest match when matching revisions
* `--no-gc` — Prevent automatic incremental garbage collection for this command; it otherwise runs in the background on writes. `lore repository gc` always runs a full pass regardless
* `--sync-data` — Force sync data to storage media during flush
* `--cache` — Cache fragment payloads fetched from remote in the local store
* `--non-interactive` — Disable interactive prompts (e.g., per-link commit messages)



## `lore repository`

Repository commands

**Usage:** `lore repository <COMMAND>`

###### **Subcommands:**

* `status` — Show current repository status
* `info` — Get info about a repository
* `list` — List repositories
* `create` — Create a repository in the given directory
* `clone` — Clone a remote repository into the given path
* `delete` — Delete a repository
* `verify` — Verify repository state consistency
* `dump` — Dump repository state information
* `gc` — Run a full garbage collection pass on the local repository store
* `store` — Access the repository data store
* `metadata` — Repository metadata operations
* `instance` — Instance management
* `config` — Read a configuration value
* `update-path` — Update the stored path for this instance



## `lore repository status`

Show current repository status.

Reports the staged revision (if any) and the files currently marked dirty. No filesystem walk runs by default — pass `--scan` to walk the filesystem and refresh dirty flags. See `lore status --help` (top-level alias) for the full workflow.

**Usage:** `lore repository status [OPTIONS] [PATH]...`

###### **Arguments:**

* `<PATH>` — Optional paths in repository

###### **Options:**

* `--scan` — Walk the filesystem under the given paths and reconcile every file against the current revision.

   Detected modifications, adds, and deletes are marked dirty; stale dirty flags are cleared. The refreshed flags are persisted in the staged state so subsequent `lore stage` and `lore status` calls see an accurate picture without rescanning.

   Without `--scan`, status reports only what is currently tracked: the staged revision (if any) plus files already marked dirty. Mark files individually with `lore dirty` for targeted updates, or pass `--scan` here for bulk reconciliation.
* `--check-dirty` — Verify already-dirty files against the filesystem without a full scan.

   Each file currently marked dirty is re-checked: one whose on-disk content still matches the tracked revision (same size, and same content when the modification time differs) has its dirty flag cleared and is dropped from the report, unless it is also staged. Adds, moves, copies, and deletes are always reported. The refreshed flags are persisted, so this requires write access.
* `--reset` — Drop the existing staged anchor before computing status. Combine with --scan to scan from a clean slate
* `--revision-only` — Only show revision info, skip all diffs
* `--count` — Count directories and files (staged state if present, else current revision; view-filtered)
* `--targets <file>` — Path to a targets file



## `lore repository info`

Get info about a repository

**Usage:** `lore repository info [url]`

###### **Arguments:**

* `<url>` — URL of repository



## `lore repository list`

List repositories

**Usage:** `lore repository list <url>`

###### **Arguments:**

* `<url>` — URL of remote



## `lore repository create`

Create a repository in the given directory

**Usage:** `lore repository create [OPTIONS] <url>`

###### **Arguments:**

* `<url>` — URL of repository

###### **Options:**

* `--description <description>` — Optional description of repository
* `--id <id>` — Optional ID of repository
* `--use-shared-store` — Use the shared store rather than create a local immutable store
* `--shared-store-path <SHARED_STORE_PATH>` — Use this path rather than the system default as the shared store location



## `lore repository clone`

Clone a remote repository into the given path

**Usage:** `lore repository clone [OPTIONS] <url> [path]`

###### **Arguments:**

* `<url>` — URL of repository
* `<path>` — Path to clone into

###### **Options:**

* `--view <view>` — Optional client side view filter file
* `--revision <revision>` — Optional revision to sync
* `--branch <branch>` — Optional branch to sync (shorthand for a full revision specifier)
* `--bare` — Clone without files, only fetch latest revision tree
* `--virtual` — Clone virtually using split-write filesystem
* `--direct-file-write` — Write directly to the destination file instead of write to a temporary file and move into place
* `--direct-file-io` — Use direct file I/O instead of memory mapping files
* `--layer <repository>` — Layer to add
* `--layer-metadata <key>` — Metadata key to link layer revisions with
* `--prefetch <file>` — File containing list of files to prefetch
* `--use-shared-store` — Use the shared store rather than create a local immutable store
* `--shared-store-path <SHARED_STORE_PATH>` — Use this path rather than the system default as the shared store location
* `--no-tracking` — Clone without local repository tracking (memory-only stores)
* `--root-file <path>` — Root files for dependency-based selective clone (only clone these files and their dependencies)
* `--dependency-tag <tag>` — Tags to filter dependencies by during dependency-based clone
* `--dependency-recursive` — Follow transitive dependencies recursively during dependency-based clone
* `--dependency-depth-limit <depth>` — Maximum dependency traversal depth (0 means unlimited)

  Default value: `0`



## `lore repository delete`

Delete a repository

**Usage:** `lore repository delete <url>`

###### **Arguments:**

* `<url>` — URL of repository



## `lore repository verify`

Verify repository state consistency

**Usage:** `lore repository verify [OPTIONS] [COMMAND]`

###### **Subcommands:**

* `state` — Verify repository state consistency (default)
* `fragment` — Verify a specific fragment in the local store

###### **Options:**

* `--path <path>` — Optional path in the repository to start verification from (for state verification)
* `--heal` — Attempt to heal discrepancies found in a new staged state



## `lore repository verify state`

Verify repository state consistency (default)

**Usage:** `lore repository verify state [OPTIONS]`

###### **Options:**

* `--path <path>` — Optional path in the repository to start verification from
* `--heal` — Attempt to heal discrepancies found in a new staged state



## `lore repository verify fragment`

Verify a specific fragment in the local store

**Usage:** `lore repository verify fragment [OPTIONS] <HASH>`

###### **Arguments:**

* `<HASH>` — Fragment hash to verify

###### **Options:**

* `--context <CONTEXT>` — Context part of the address to verify
* `--heal` — Attempt to heal if verification fails (remote only)



## `lore repository dump`

Dump repository state information

**Usage:** `lore repository dump [OPTIONS]`

###### **Options:**

* `--path <path>` — Optional path in the repository to start dumping from
* `--revision <revision>` — Optional revision to dump
* `--max-depth <max-depth>` — Optional max depth of tree dump



## `lore repository gc`

Run a full garbage collection pass on the local repository store

**Usage:** `lore repository gc`



## `lore repository store`

Access the repository data store

**Usage:** `lore repository store <COMMAND>`

###### **Subcommands:**

* `immutable` — Operations on the immutable store



## `lore repository store immutable`

Operations on the immutable store

**Usage:** `lore repository store immutable <COMMAND>`

###### **Subcommands:**

* `query` — Query the store



## `lore repository store immutable query`

Query the store

**Usage:** `lore repository store immutable query [OPTIONS] <ADDRESS>`

###### **Arguments:**

* `<ADDRESS>` — Fragment address to query

###### **Options:**

* `--recurse` — Recurse into subfragments



## `lore repository metadata`

Repository metadata operations

**Usage:** `lore repository metadata <COMMAND>`

###### **Subcommands:**

* `get` — Get metadata from the repository (omit key to list all)
* `set` — Set metadata on the repository
* `clear` — Clear metadata from the repository



## `lore repository metadata get`

Get metadata from the repository (omit key to list all)

**Usage:** `lore repository metadata get [key]`

###### **Arguments:**

* `<key>` — Attribute to get (omit to list all)



## `lore repository metadata set`

Set metadata on the repository

**Usage:** `lore repository metadata set [OPTIONS] [pairs]...`

###### **Arguments:**

* `<pairs>` — Metadata key/value pairs

###### **Options:**

* `--binary` — Indicator that values are paths to binary files
* `--numeric` — Indicator that values are numeric (u64)



## `lore repository metadata clear`

Clear metadata from the repository

**Usage:** `lore repository metadata clear [keys]...`

###### **Arguments:**

* `<keys>` — Keys to clear (omit to clear all user-defined keys)



## `lore repository instance`

Instance management

**Usage:** `lore repository instance <COMMAND>`

###### **Subcommands:**

* `list` — List all registered instances for this repository
* `prune` — Remove stale instance entries



## `lore repository instance list`

List all registered instances for this repository

**Usage:** `lore repository instance list`



## `lore repository instance prune`

Remove stale instance entries

**Usage:** `lore repository instance prune`



## `lore repository config`

Read a configuration value

**Usage:** `lore repository config <COMMAND>`

###### **Subcommands:**

* `get` — Get a configuration value



## `lore repository config get`

Get a configuration value

**Usage:** `lore repository config get <KEY>`

###### **Arguments:**

* `<KEY>` — The configuration key to read



## `lore repository update-path`

Update the stored path for this instance

**Usage:** `lore repository update-path`



## `lore branch`

Branch commands

**Usage:** `lore branch <COMMAND>`

###### **Subcommands:**

* `list` — List available branches
* `info` — Get info about the given branch
* `create` — Create a new branch
* `switch` — Switch to a different branch
* `push` — Push commits to remote
* `merge` — Merge two branches
* `diff` — Diff two branches using the common ancestor base revision Will calculate the set of changes between source branch latest revision and the base revision that is not in the set of changes between the target branch latest revision and the base revision
* `archive` — Archive an existing branch
* `reset` — Reset local latest pointer for a branch
* `protect` — Protect a branch from direct pushes
* `unprotect` — Remove push protection from a branch
* `latest` — Branch latest related commands
* `metadata` — Branch metadata operations



## `lore branch list`

List available branches

**Usage:** `lore branch list [OPTIONS]`

###### **Options:**

* `--archived` — Include archived local branches



## `lore branch info`

Get info about the given branch

**Usage:** `lore branch info [branch]`

###### **Arguments:**

* `<branch>` — Name of the branch



## `lore branch create`

Create a new branch

**Usage:** `lore branch create [OPTIONS] <branch>`

###### **Arguments:**

* `<branch>` — Name of the branch

###### **Options:**

* `--id <id>` — Optional explicit branch ID (hex-encoded 16-byte identifier)



## `lore branch switch`

Switch to a different branch

**Usage:** `lore branch switch [OPTIONS] <branch> [revision]`

###### **Arguments:**

* `<branch>` — Name of the branch
* `<revision>` — Revision to switch to

###### **Options:**

* `--dry-run` — Do a dry run sync and only report what changes would be done, do not change anything in the file system
* `--local` — Keep last local latest revision, do not sync latest revision from remote (implied by offline mode)
* `--reset` — Reset any local modified files to match the incoming revision
* `--bare` — Only update anchor tracking without modifying or verifying files, useful for bare repositories



## `lore branch push`

Push commits to remote

**Usage:** `lore branch push [OPTIONS] [branch]`

###### **Arguments:**

* `<branch>` — Optional name or identifier of the branch, push current branch if not specified

###### **Options:**

* `--fast-forward-merge` — Allow the server to fast-forward merge if the target branch head has moved



## `lore branch merge`

Merge two branches

**Usage:** `lore branch merge [OPTIONS] <branch|--id <branch-id>>
       merge [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `unresolve` — Marks the merge unresolved
* `into` — Merge into branch
* `start` — Start a merge process
* `restart` — Restart the merge, resetting the current merge state
* `resolve` — Resolves the merge
* `abort` — Abort a merge process

###### **Arguments:**

* `<branch>` — Name of the source branch to merge into the current branch

###### **Options:**

* `--id <branch-id>` — ID of the source branch to merge into the current branch
* `--message <MESSAGE>` — Change the message for committing when no conflicts arise from the merge



## `lore branch merge unresolve`

Marks the merge unresolved

**Usage:** `lore branch merge unresolve <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to unresolve

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore branch merge into`

Merge into branch

**Usage:** `lore branch merge into [OPTIONS] <branch|--id <branch-id>> <MESSAGE>`

###### **Arguments:**

* `<branch>` — Name of the target branch to merge the current branch into
* `<MESSAGE>` — Commit message

###### **Options:**

* `--id <branch-id>` — ID of the target branch to merge the current branch into
* `--link <LINK>` — Merge only a specific linked repository at the given mount path
* `--ignore-links` — Merge only the main repository, skipping all linked repositories



## `lore branch merge start`

Start a merge process

**Usage:** `lore branch merge start [OPTIONS] <branch|--id <branch-id>>`

###### **Arguments:**

* `<branch>` — Name of the source branch to merge into the current branch

###### **Options:**

* `--id <branch-id>` — ID of the source branch to merge into the current branch
* `--message <MESSAGE>` — Change the message for committing when no conflicts arise from the merge
* `--no-commit` — Disable auto commits even if no conflicts arise from the merge
* `--dry-run` — Do a dry run merge start and only report what changes would be done, do not change anything in the file system
* `--link <LINK>` — Merge only a specific linked repository at the given mount path
* `--ignore-links` — Merge only the main repository, skipping all linked repositories



## `lore branch merge restart`

Restart the merge, resetting the current merge state

**Usage:** `lore branch merge restart <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to restart

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore branch merge resolve`

Resolves the merge

**Usage:** `lore branch merge resolve [OPTIONS] [paths]...
       resolve <COMMAND>`

###### **Subcommands:**

* `mine` — Resolve using my changes
* `theirs` — Resolve using their changes

###### **Arguments:**

* `<paths>` — Any number of paths or files to reset

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore branch merge resolve mine`

Resolve using my changes

**Usage:** `lore branch merge resolve mine <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to stage

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore branch merge resolve theirs`

Resolve using their changes

**Usage:** `lore branch merge resolve theirs <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to stage

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore branch merge abort`

Abort a merge process

**Usage:** `lore branch merge abort [OPTIONS]`

###### **Options:**

* `--link <LINK>` — Abort only a specific linked repository merge at the given mount path
* `--ignore-links` — Abort only the main repository merge, keeping link pin updates



## `lore branch diff`

Diff two branches using the common ancestor base revision Will calculate the set of changes between source branch latest revision and the base revision that is not in the set of changes between the target branch latest revision and the base revision

**Usage:** `lore branch diff [OPTIONS] <target>`

###### **Arguments:**

* `<target>` — Name of the target branch

###### **Options:**

* `--source <source>` — Name of the source branch
* `--auto-resolve` — Attempt to auto resolve conflicts if true



## `lore branch archive`

Archive an existing branch

**Usage:** `lore branch archive <branch>`

###### **Arguments:**

* `<branch>` — Name of the branch to archive



## `lore branch reset`

Reset local latest pointer for a branch

**Usage:** `lore branch reset [OPTIONS] <revision>`

###### **Arguments:**

* `<revision>` — Revision to reset the local latest pointer to

###### **Options:**

* `--branch <branch>` — Branch to reset, or the current branch if not set



## `lore branch protect`

Protect a branch from direct pushes

**Usage:** `lore branch protect <branch>`

###### **Arguments:**

* `<branch>` — Name of the branch to protect



## `lore branch unprotect`

Remove push protection from a branch

**Usage:** `lore branch unprotect <branch>`

###### **Arguments:**

* `<branch>` — Name of the branch to unprotect



## `lore branch latest`

Branch latest related commands

**Usage:** `lore branch latest <COMMAND>`

###### **Subcommands:**

* `list` —



## `lore branch latest list`

**Usage:** `lore branch latest list [OPTIONS] [LIMIT]`

###### **Arguments:**

* `<LIMIT>` — Max number of history entries to show

###### **Options:**

* `--branch <branch>` — Branch to query



## `lore branch metadata`

Branch metadata operations

**Usage:** `lore branch metadata <COMMAND>`

###### **Subcommands:**

* `get` — Get metadata from the branch (omit key to list all)
* `set` — Set metadata on the branch
* `clear` — Clear metadata from the branch



## `lore branch metadata get`

Get metadata from the branch (omit key to list all)

**Usage:** `lore branch metadata get [OPTIONS] [key]`

###### **Arguments:**

* `<key>` — Attribute to get (omit to list all)

###### **Options:**

* `--branch <branch>` — Branch name (uses current branch if not specified)



## `lore branch metadata set`

Set metadata on the branch

**Usage:** `lore branch metadata set [OPTIONS] [pairs]...`

###### **Arguments:**

* `<pairs>` — Metadata key/value pairs

###### **Options:**

* `--binary` — Indicator that values are paths to binary files
* `--numeric` — Indicator that values are numeric (u64)
* `--branch <branch>` — Branch name (uses current branch if not specified)



## `lore branch metadata clear`

Clear metadata from the branch

**Usage:** `lore branch metadata clear [OPTIONS] [keys]...`

###### **Arguments:**

* `<keys>` — Keys to clear (omit to clear all user-defined keys)

###### **Options:**

* `--branch <branch>` — Branch name (uses current branch if not specified)



## `lore revision`

Revision commands

**Usage:** `lore revision <COMMAND>`

###### **Subcommands:**

* `history` — List revisions of a repository
* `info` — Get info about a revision
* `commit` — Commit the staged state
* `amend` — Amend the latest commit's message
* `sync` — Synchronize to a given state of a repository
* `bisect` — Binary search for a change introduced between start (exclusive) and end (inclusive.)
* `diff` — Diff two revisions
* `find` — Find revision
* `restore` — Restore current revision as latest revision
* `cherry-pick` — Cherry-pick a revision onto the currently synced revision
* `revert` — Revert a revision from the currently synced revision
* `metadata` — Manage metadata of a given revision



## `lore revision history`

List revisions of a repository

**Usage:** `lore revision history [OPTIONS] [LENGTH]`

###### **Arguments:**

* `<LENGTH>` — Number of revisions to show

###### **Options:**

* `--revision <revision>` — Start listing from the specified revision. If not specified, start listing from the current branch latest revision
* `--branch <branch>` — Show branch revisions
* `--only-branch` — Stop when reaching a revision on a different branch (includes the branch point revision)
* `--oneline` — Output each revision on one line only



## `lore revision info`

Get info about a revision

**Usage:** `lore revision info [OPTIONS] [revision]`

###### **Arguments:**

* `<revision>` — Revision to get info for

###### **Options:**

* `--delta` — Show delta information
* `--metadata` — Show file metadata information



## `lore revision commit`

Commit the staged state

**Usage:** `lore revision commit [OPTIONS] <MESSAGE>`

###### **Arguments:**

* `<MESSAGE>` — Commit message

###### **Options:**

* `--stats` — Print stats
* `--link <LINK>` — Commit only changes in this linked repository (mount path relative to repo root)
* `--link-message <PATH>` — Per-link commit message. Takes two values: <path> <message>. Can be specified multiple times
* `--layer <LAYER>` — Commit only changes in this layer (mount path relative to repo root)
* `--layer-message <PATH>` — Per-layer commit message. Takes two values: <path> <message>. Can be specified multiple times



## `lore revision amend`

Amend the latest commit's message

**Usage:** `lore revision amend [OPTIONS] <MESSAGE>`

###### **Arguments:**

* `<MESSAGE>` — Commit message

###### **Options:**

* `--stats` — Print stats



## `lore revision sync`

Synchronize to a given state of a repository

**Usage:** `lore revision sync [OPTIONS] [revision]`

**Command Alias:** `synchronize`

###### **Arguments:**

* `<revision>` — Revision hash signature to synchronize to. Can be a signature on any branch — if the target revision is on a different branch, the current branch is updated accordingly. Can be a partial hash signature

###### **Options:**

* `--forward-changes` — Fast forward any local changes if syncing to a local revision
* `--reset` — Reset any local modified files to match the incoming revision
* `--root-file <path>` — Root files for dependency-based selective sync (only sync changes for these files and their dependencies)
* `--dependency-tag <tag>` — Tags to filter dependencies by during dependency-based sync
* `--dependency-recursive` — Follow transitive dependencies recursively during dependency-based sync
* `--dependency-depth-limit <depth>` — Maximum dependency traversal depth (0 means unlimited)

  Default value: `0`



## `lore revision bisect`

Binary search for a change introduced between start (exclusive) and end (inclusive.)

**Usage:** `lore revision bisect --start <start_revision> --end <end_revision>`

###### **Options:**

* `--start <start_revision>` — The latest revision known to not have the change
* `--end <end_revision>` — The earliest revision known to have the change



## `lore revision diff`

Diff two revisions

**Usage:** `lore revision diff [OPTIONS] <revision_source>`

###### **Arguments:**

* `<revision_source>` — Source revision to compare

###### **Options:**

* `--target <revision_target>` — Target revision to compare, by default the current revision
* `--path <PATH>` — Optional path in repository
* `--targets <file>` — Path to a targets file



## `lore revision find`

Find revision

**Usage:** `lore revision find <COMMAND>`

###### **Subcommands:**

* `metadata` — Find revision by metadata
* `number` — Find revision by number



## `lore revision find metadata`

Find revision by metadata

**Usage:** `lore revision find metadata <key> [value]`

###### **Arguments:**

* `<key>` — Metadata key to search for
* `<value>` — Metadata value to match with



## `lore revision find number`

Find revision by number

**Usage:** `lore revision find number <NUMBER>`

###### **Arguments:**

* `<NUMBER>` — Revision number to search for



## `lore revision restore`

Restore current revision as latest revision

**Usage:** `lore revision restore [MESSAGE]`

###### **Arguments:**

* `<MESSAGE>` — Commit message



## `lore revision cherry-pick`

Cherry-pick a revision onto the currently synced revision

**Usage:** `lore revision cherry-pick [OPTIONS] <revision>
       cherry-pick [OPTIONS] [revision] <COMMAND>`

###### **Subcommands:**

* `unresolve` — Marks the cherry-pick unresolved
* `restart` — Restart the cherry-pick, resetting the current cherry-pick state
* `resolve` — Resolve conflicts
* `abort` — Abort a cherry-pick

###### **Arguments:**

* `<revision>` — Target revision to cherry-pick

###### **Options:**

* `--message <MESSAGE>` — Change the message for committing when no conflicts arise from the cherry-pick
* `--no-commit` — Disable auto commits even if no conflicts arise from the cherry-pick



## `lore revision cherry-pick unresolve`

Marks the cherry-pick unresolved

**Usage:** `lore revision cherry-pick unresolve <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision cherry-pick restart`

Restart the cherry-pick, resetting the current cherry-pick state

**Usage:** `lore revision cherry-pick restart <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision cherry-pick resolve`

Resolve conflicts

**Usage:** `lore revision cherry-pick resolve <paths|--targets <file>>
       resolve <COMMAND>`

###### **Subcommands:**

* `mine` — Resolve using my changes
* `theirs` — Resolve using the incoming changes

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision cherry-pick resolve mine`

Resolve using my changes

**Usage:** `lore revision cherry-pick resolve mine <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision cherry-pick resolve theirs`

Resolve using the incoming changes

**Usage:** `lore revision cherry-pick resolve theirs <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision cherry-pick abort`

Abort a cherry-pick

**Usage:** `lore revision cherry-pick abort`



## `lore revision revert`

Revert a revision from the currently synced revision

**Usage:** `lore revision revert [OPTIONS] <revision>
       revert [OPTIONS] [revision] <COMMAND>`

###### **Subcommands:**

* `unresolve` — Marks the revert unresolved
* `restart` — Restart the revert, resetting the current revert state
* `resolve` — Resolve conflicts
* `abort` — Abort a revert

###### **Arguments:**

* `<revision>` — Target revision to revert

###### **Options:**

* `--message <MESSAGE>` — Change the message for committing when no conflicts arise from the revert
* `--no-commit` — Disable auto commits even if no conflicts arise from the revert



## `lore revision revert unresolve`

Marks the revert unresolved

**Usage:** `lore revision revert unresolve <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision revert restart`

Restart the revert, resetting the current revert state

**Usage:** `lore revision revert restart <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision revert resolve`

Resolve conflicts

**Usage:** `lore revision revert resolve <paths|--targets <file>>
       resolve <COMMAND>`

###### **Subcommands:**

* `mine` — Resolve using my changes
* `theirs` — Resolve using the incoming changes

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision revert resolve mine`

Resolve using my changes

**Usage:** `lore revision revert resolve mine <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision revert resolve theirs`

Resolve using the incoming changes

**Usage:** `lore revision revert resolve theirs <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to target

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore revision revert abort`

Abort a revert

**Usage:** `lore revision revert abort`



## `lore revision metadata`

Manage metadata of a given revision

**Usage:** `lore revision metadata <COMMAND>`

###### **Subcommands:**

* `clear` — Clear metadata for a staged revision
* `get` — Get metadata from a revision
* `set` — Set metadata on for a staged revision



## `lore revision metadata clear`

Clear metadata for a staged revision

**Usage:** `lore revision metadata clear`



## `lore revision metadata get`

Get metadata from a revision

**Usage:** `lore revision metadata get [OPTIONS] [key]`

###### **Arguments:**

* `<key>` — Attribute to get metadata for

###### **Options:**

* `--revision <revision>` — Revision to get metadata for



## `lore revision metadata set`

Set metadata on for a staged revision

**Usage:** `lore revision metadata set [OPTIONS] [pairs]...`

###### **Arguments:**

* `<pairs>` — Metadata key/value pairs

###### **Options:**

* `--binary` — Indicator that values are paths to files



## `lore file`

File commands

**Usage:** `lore file <COMMAND>`

###### **Subcommands:**

* `info` — Get info about the given file or directory
* `metadata` — Manage metadata of a given file or directory
* `dependency` — Manage file dependencies
* `stage` — Stage changes for commit
* `dirty` — Mark files as dirty so they show up in `lore status` and get picked up by directory-scoped `lore stage` (no content is read or staged)
* `unstage` — Unstage changes to a file or directory
* `reset` — Reset changes to a path or file to the current revision, discarding your local changes
* `obliterate` — Obliterate a file or fragment
* `history` — List revisions of a file
* `diff` — Show differences between two revisions of a file
* `write` — Write data to a specific location
* `hash` — Hash a local file



## `lore file info`

Get info about the given file or directory

**Usage:** `lore file info [OPTIONS] <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--targets <file>` — Path to a targets file containing all the paths to all files
* `--revision <revision>` — Revision to get info from
* `--local` — If given, calculate the local file system size and hash based on the current local filter
* `--filtered` — If given, calculate the repository size based on the current local filter



## `lore file metadata`

Manage metadata of a given file or directory

**Usage:** `lore file metadata <COMMAND>`

###### **Subcommands:**

* `clear` — Clear metadata for a staged file
* `get` — Get metadata from a file
* `set` — Set metadata on for a staged file



## `lore file metadata clear`

Clear metadata for a staged file

**Usage:** `lore file metadata clear <PATH>`

###### **Arguments:**

* `<PATH>` — File path to clear metadata for



## `lore file metadata get`

Get metadata from a file

**Usage:** `lore file metadata get [OPTIONS] <PATH> [key]`

###### **Arguments:**

* `<PATH>` — File to get metadata for
* `<key>` — Attribute to get metadata for

###### **Options:**

* `--revision <revision>` — Revision to get metadata for



## `lore file metadata set`

Set metadata on for a staged file

**Usage:** `lore file metadata set [OPTIONS] <PATH> [pairs]...`

###### **Arguments:**

* `<PATH>` — File path to set metadata on
* `<pairs>` — Metadata key/value pairs

###### **Options:**

* `--binary` — Indicator that values are paths to files



## `lore file dependency`

Manage file dependencies

**Usage:** `lore file dependency <COMMAND>`

###### **Subcommands:**

* `add` — Add dependency edges from a source file to one or more dependency files
* `remove` — Remove dependency edges from a source file to one or more dependency files
* `list` — List dependencies or dependents for files



## `lore file dependency add`

Add dependency edges from a source file to one or more dependency files

**Usage:** `lore file dependency add [OPTIONS] <SOURCE> [dependencies]...`

###### **Arguments:**

* `<SOURCE>` — Source file that depends on the listed dependencies
* `<dependencies>` — One or more dependency file paths

###### **Options:**

* `--tag <tag>` — Tags to apply to all added dependency edges
* `--force` — Skip cycle detection



## `lore file dependency remove`

Remove dependency edges from a source file to one or more dependency files

**Usage:** `lore file dependency remove [OPTIONS] <SOURCE> [dependencies]...`

###### **Arguments:**

* `<SOURCE>` — Source file to remove dependencies from
* `<dependencies>` — One or more dependency file paths to remove

###### **Options:**

* `--tag <tag>` — Remove only specific tags instead of entire edges



## `lore file dependency list`

List dependencies or dependents for files

**Usage:** `lore file dependency list [OPTIONS] [paths]...`

###### **Arguments:**

* `<paths>` — Paths to list dependencies for (all files if omitted)

###### **Options:**

* `--reverse` — List dependents instead of dependencies
* `--recursive` — Recursively resolve transitive dependencies
* `--tag <tag>` — Filter by tag
* `--depth <limit>` — Maximum recursion depth (0 = unlimited)

  Default value: `0`
* `--revision <revision>` — Revision to query (defaults to staged/current)



## `lore file stage`

Stage changes for commit.

Directory paths (including `.`) stage only files already marked dirty under that directory; clean or unmarked files are skipped. Mark files first with `lore file dirty` (or `lore status --scan` to reconcile dirty flags in bulk), or pass `--scan` here to walk the filesystem and stage in one pass.

Specific file paths are checked against the filesystem and staged if content differs from the current revision, regardless of their dirty flag.

`--scan` walks the filesystem under the given paths, marks every detected modification/add/delete dirty, and stages them in one step.

**Usage:** `lore file stage [OPTIONS] <paths|--targets <file>>
       stage [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `move` — Move or rename a file or directory
* `merge` — Stage as a merge

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--case <case>` — Case change handling

  Possible values:
  - `error`:
    Generate error on case mismatch
  - `keep`:
    Keep current case in repository (update file system)
  - `rename`:
    Rename case in repository (keep file system)

* `--scan` — Walk the filesystem under the given paths to detect modified, added, and deleted files.

   Detected changes are marked dirty and staged in a single pass. Use this when changes were made externally (without going through `lore dirty`), or to recover after losing track of dirty state. Equivalent in effect to running `lore status --scan` followed by `lore stage`, but performed in one traversal.

   Without `--scan`, directory staging stages only files already marked dirty under that directory — mark them first with `lore dirty <paths>`, or run `lore status --scan` to reconcile dirty flags across a tree. Single-file stage paths are always checked against the filesystem regardless of this flag.
* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore file stage move`

Move or rename a file or directory

**Usage:** `lore file stage move <from> <to>`

###### **Arguments:**

* `<from>` — Original path of file
* `<to>` — New path of file



## `lore file stage merge`

Stage as a merge

**Usage:** `lore file stage merge <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore file dirty`

Mark files as dirty so they show up in `lore status` and get picked up by directory-scoped `lore stage` (no content is read or staged).

Use when files were changed externally and you want to notify Lore of specific paths without performing a full filesystem walk. For bulk reconciliation across a tree, prefer `lore status --scan` or `lore stage --scan`.

**Usage:** `lore file dirty [OPTIONS] [paths]... [COMMAND]`

###### **Subcommands:**

* `move` — Mark a file as moved (dirty)
* `copy` — Mark a file as copied (dirty)

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore file dirty move`

Mark a file as moved (dirty)

**Usage:** `lore file dirty move <from> <to>`

###### **Arguments:**

* `<from>` — Original path of file
* `<to>` — New path of file



## `lore file dirty copy`

Mark a file as copied (dirty)

**Usage:** `lore file dirty copy <from> <to>`

###### **Arguments:**

* `<from>` — Source path of file
* `<to>` — Destination path of copy



## `lore file unstage`

Unstage changes to a file or directory

**Usage:** `lore file unstage <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to unstage

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore file reset`

Reset changes to a path or file to the current revision, discarding your local changes

**Usage:** `lore file reset [OPTIONS] <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--purge` — Delete untracked files
* `--targets <file>` — Path to a targets file containing all the paths to all files
* `--revision <revision>` — Revision to reset files to
* `--last-merged-from <branch>` — If given, the files will be reset to the last point of merge from this branch, or the branch point from this branch if no merge has been performed



## `lore file obliterate`

Obliterate a file or fragment

**Usage:** `lore file obliterate <--address <ADDRESS>|--path <PATH>>`

###### **Options:**

* `--address <ADDRESS>` — Address of a blob
* `--path <PATH>` — Path to a file



## `lore file history`

List revisions of a file

**Usage:** `lore file history [OPTIONS] <PATH> [LENGTH]`

###### **Arguments:**

* `<PATH>` — File path to get revisions for
* `<LENGTH>` — Number of revisions to show

###### **Options:**

* `--revision <revision>` — Revision to start from
* `--branch <branch>` — Show branch revisions
* `--depth <depth>` — Number of revisions to search initially
* `--oneline` — Output each revision on one line only



## `lore file diff`

Show differences between two revisions of a file

**Usage:** `lore file diff [OPTIONS] [paths]...`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--source <revision_source>` — Optional signature of the source revision to diff from, by default the current revision
* `--target <revision_target>` — Optional signature of the target revision to diff to, by default the current file system state
* `--diff3` — If given, produce three-way merge output with conflict markers instead of a two-way unified diff
* `-U`, `--context <n>` — Number of unchanged context lines to show around each hunk

  Default value: `3`
* `--ignore-space-at-eol` — Treat lines that differ only in trailing whitespace as unchanged
* `--ignore-space-change` — Collapse runs of internal whitespace to a single space before comparing
* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore file write`

Write data to a specific location

**Usage:** `lore file write [OPTIONS] --output <OUTPUT>`

###### **Options:**

* `--address <ADDRESS>` — Address of a blob
* `--path <PATH>` — Path to a file
* `--revision <REVISION>` — Revision specifier
* `--output <OUTPUT>` — Path to a destination



## `lore file hash`

Hash a local file

**Usage:** `lore file hash [OPTIONS] [paths]...`

###### **Arguments:**

* `<paths>` — Any number of paths or files to unstage

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore auth`

Authentication commands

**Usage:** `lore auth <COMMAND>`

###### **Subcommands:**

* `login` — Authenticate the CLI
* `info` — Display identity information for the current user or specified user IDs
* `list` — List all stored authentication identities
* `logout` — Remove stored authentication and authorization tokens
* `clear` — Clear all stored authentication data



## `lore auth login`

Authenticate the CLI

**Usage:** `lore auth login [OPTIONS] [remote-url]`

###### **Arguments:**

* `<remote-url>` — Server URL

###### **Options:**

* `--token-type <TOKEN_TYPE>` — Token type for non-interactive login (e.g. "api-key", "eg1", "lore")
* `--token <TOKEN>` — Token value for non-interactive login (requires --token-type)
* `--auth-url <AUTH_URL>` — Auth service URL with scheme (e.g. `ucs-auth://auth.example.com`). Required when logging in with `--token` outside a repository without a remote-url
* `--no-browser` — Avoid opening a browser to login



## `lore auth info`

Display identity information for the current user or specified user IDs

**Usage:** `lore auth info [OPTIONS] [user-id]...`

###### **Arguments:**

* `<user-id>` — User IDs to resolve (omit for current user)

###### **Options:**

* `--with-token` — Include cached tokens in the output



## `lore auth list`

List all stored authentication identities

**Usage:** `lore auth list [OPTIONS]`

###### **Options:**

* `--with-token` — Include cached tokens in the output



## `lore auth logout`

Remove stored authentication and authorization tokens

**Usage:** `lore auth logout [OPTIONS]`

###### **Options:**

* `--auth-url <auth-url>` — Auth service URL (omit to use current repository's auth URL)
* `--resource <resource>` — Resource ID to remove a specific authorization (e.g. "urc-{id}")
* `--user-id <user-id>` — User ID to remove (omit to remove all identities)



## `lore auth clear`

Clear all stored authentication data

**Usage:** `lore auth clear`



## `lore layer`

Layer commands

**Usage:** `lore layer <COMMAND>`

###### **Subcommands:**

* `add` — Add a repository layer
* `remove` — Remove a repository layer
* `list` — List repository layers



## `lore layer add`

Add a repository layer

**Usage:** `lore layer add [OPTIONS] <path> <repository> <path>`

###### **Arguments:**

* `<path>` — Path in the current repository where the layer should be placed
* `<repository>` — Repository to add as a layer, either an ID or a name
* `<path>` — Path in the layer repository where the layer should start

###### **Options:**

* `--metadata <metadata>` — Metadata key to use for matching revisions



## `lore layer remove`

Remove a repository layer

**Usage:** `lore layer remove [OPTIONS] <path> [repository]`

###### **Arguments:**

* `<path>` — Path in the current repository where the layer is placed
* `<repository>` — Repository placed as a layer. Optional when the target path matches a single configured layer; required to disambiguate when multiple layers share the same target path

###### **Options:**

* `--purge` — Also delete untracked files and all directories inside the layer mount



## `lore layer list`

List repository layers

**Usage:** `lore layer list`



## `lore logfile`

Logfile commands

**Usage:** `lore logfile <COMMAND>`

###### **Subcommands:**

* `info` — Info



## `lore logfile info`

Info

**Usage:** `lore logfile info`



## `lore login`

Authenticate the CLI

**Usage:** `lore login [OPTIONS] [remote-url]`

###### **Arguments:**

* `<remote-url>` — Server URL

###### **Options:**

* `--token-type <TOKEN_TYPE>` — Token type for non-interactive login (e.g. "api-key", "eg1", "lore")
* `--token <TOKEN>` — Token value for non-interactive login (requires --token-type)
* `--auth-url <AUTH_URL>` — Auth service URL with scheme (e.g. `ucs-auth://auth.example.com`). Required when logging in with `--token` outside a repository without a remote-url
* `--no-browser` — Avoid opening a browser to login



## `lore link`

Link commands

**Usage:** `lore link <COMMAND>`

###### **Subcommands:**

* `add` — Link to the given point in the repository and subpath from the given repository
* `remove` — Remove the link at the given point in the repository
* `update` — Update the link to a new pin
* `list` — List all links in the repository



## `lore link add`

Link to the given point in the repository and subpath from the given repository

**Usage:** `lore link add [OPTIONS] <link_path> <link_url> <source_path>`

###### **Arguments:**

* `<link_path>` — Path in the current repository where the repository should be linked in
* `<link_url>` — Repository URL
* `<source_path>` — Path in the link repository that should be linked in

###### **Options:**

* `--pin <pin>` — Branch or specific revision to pin the link to, defaulting to latest on the main branch
* `--disable-branching` — Disable automatic branch creation in the linked repository



## `lore link remove`

Remove the link at the given point in the repository

**Usage:** `lore link remove <link_path>`

###### **Arguments:**

* `<link_path>` — Path in the current repository where the module is linked in



## `lore link update`

Update the link to a new pin

**Usage:** `lore link update [OPTIONS] <link_path>`

###### **Arguments:**

* `<link_path>` — Path in the repository where the link should be updated

###### **Options:**

* `--pin <pin>` — Branch or specific revision to pin the link to, defaulting to latest on the current branch



## `lore link list`

List all links in the repository

**Usage:** `lore link list [OPTIONS]`

###### **Options:**

* `--staged` — Only show links with staged changes



## `lore status`

Show current repository status.

Reports the staged revision (if any) plus the files and directories currently marked dirty. By default no filesystem walk is performed — only the tracked dirty flags are read, so changes made without prior `lore dirty` or `--scan` will not appear.

Pass `--scan` to walk the filesystem under the given paths, reconcile every file against the current revision, and refresh dirty flags (setting them on detected modifications/adds/deletes and clearing stale ones). The refreshed flags are persisted so subsequent `lore stage` / `lore status` calls see an accurate picture without rescanning.

**Usage:** `lore status [OPTIONS] [PATH]...`

###### **Arguments:**

* `<PATH>` — Optional paths in repository

###### **Options:**

* `--scan` — Walk the filesystem under the given paths and reconcile every file against the current revision.

   Detected modifications, adds, and deletes are marked dirty; stale dirty flags are cleared. The refreshed flags are persisted in the staged state so subsequent `lore stage` and `lore status` calls see an accurate picture without rescanning.

   Without `--scan`, status reports only what is currently tracked: the staged revision (if any) plus files already marked dirty. Mark files individually with `lore dirty` for targeted updates, or pass `--scan` here for bulk reconciliation.
* `--check-dirty` — Verify already-dirty files against the filesystem without a full scan.

   Each file currently marked dirty is re-checked: one whose on-disk content still matches the tracked revision (same size, and same content when the modification time differs) has its dirty flag cleared and is dropped from the report, unless it is also staged. Adds, moves, copies, and deletes are always reported. The refreshed flags are persisted, so this requires write access.
* `--reset` — Drop the existing staged anchor before computing status. Combine with --scan to scan from a clean slate
* `--revision-only` — Only show revision info, skip all diffs
* `--count` — Count directories and files (staged state if present, else current revision; view-filtered)
* `--targets <file>` — Path to a targets file



## `lore clone`

Clone a remote repository into the given path

**Usage:** `lore clone [OPTIONS] <url> [path]`

###### **Arguments:**

* `<url>` — URL of repository
* `<path>` — Path to clone into

###### **Options:**

* `--view <view>` — Optional client side view filter file
* `--revision <revision>` — Optional revision to sync
* `--branch <branch>` — Optional branch to sync (shorthand for a full revision specifier)
* `--bare` — Clone without files, only fetch latest revision tree
* `--virtual` — Clone virtually using split-write filesystem
* `--direct-file-write` — Write directly to the destination file instead of write to a temporary file and move into place
* `--direct-file-io` — Use direct file I/O instead of memory mapping files
* `--layer <repository>` — Layer to add
* `--layer-metadata <key>` — Metadata key to link layer revisions with
* `--prefetch <file>` — File containing list of files to prefetch
* `--use-shared-store` — Use the shared store rather than create a local immutable store
* `--shared-store-path <SHARED_STORE_PATH>` — Use this path rather than the system default as the shared store location
* `--no-tracking` — Clone without local repository tracking (memory-only stores)
* `--root-file <path>` — Root files for dependency-based selective clone (only clone these files and their dependencies)
* `--dependency-tag <tag>` — Tags to filter dependencies by during dependency-based clone
* `--dependency-recursive` — Follow transitive dependencies recursively during dependency-based clone
* `--dependency-depth-limit <depth>` — Maximum dependency traversal depth (0 means unlimited)

  Default value: `0`



## `lore stage`

Stage changes for commit.

Directory path (including `.`): stages only files already marked dirty under that directory. No filesystem walk is performed; clean or unmarked files are skipped. Mark files first with `lore dirty` (or `lore status --scan` to reconcile in bulk), or pass `--scan` here to discover and stage in one pass.

Specific file path: checked against the filesystem and staged if its on-disk content differs from the current revision, regardless of its dirty flag.

`--scan`: forces a filesystem walk under the given paths, marks modified, added, and deleted files dirty, and stages them in one step. Use this when changes were made externally without going through `lore dirty`, or to recover after losing track of dirty state.

**Usage:** `lore stage [OPTIONS] <paths|--targets <file>>
       stage [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `move` — Move or rename a file or directory
* `merge` — Stage as a merge

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--case <case>` — Case change handling

  Possible values:
  - `error`:
    Generate error on case mismatch
  - `keep`:
    Keep current case in repository (update file system)
  - `rename`:
    Rename case in repository (keep file system)

* `--scan` — Walk the filesystem under the given paths to detect modified, added, and deleted files.

   Detected changes are marked dirty and staged in a single pass. Use this when changes were made externally (without going through `lore dirty`), or to recover after losing track of dirty state. Equivalent in effect to running `lore status --scan` followed by `lore stage`, but performed in one traversal.

   Without `--scan`, directory staging stages only files already marked dirty under that directory — mark them first with `lore dirty <paths>`, or run `lore status --scan` to reconcile dirty flags across a tree. Single-file stage paths are always checked against the filesystem regardless of this flag.
* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore stage move`

Move or rename a file or directory

**Usage:** `lore stage move <from> <to>`

###### **Arguments:**

* `<from>` — Original path of file
* `<to>` — New path of file



## `lore stage merge`

Stage as a merge

**Usage:** `lore stage merge <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore dirty`

Mark files as dirty so they show up in `lore status` and get picked up by `lore stage` (no content is read or staged).

Use this when your editor or build tool has modified files and you want to inform Lore of the change without performing a full `--scan`. For bulk reconciliation across a tree, prefer `lore status --scan` or `lore stage --scan`.

**Usage:** `lore dirty [OPTIONS] [paths]... [COMMAND]`

###### **Subcommands:**

* `move` — Mark a file as moved (dirty)
* `copy` — Mark a file as copied (dirty)

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore dirty move`

Mark a file as moved (dirty)

**Usage:** `lore dirty move <from> <to>`

###### **Arguments:**

* `<from>` — Original path of file
* `<to>` — New path of file



## `lore dirty copy`

Mark a file as copied (dirty)

**Usage:** `lore dirty copy <from> <to>`

###### **Arguments:**

* `<from>` — Source path of file
* `<to>` — Destination path of copy



## `lore unstage`

Unstage changes to a file or directory

**Usage:** `lore unstage <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files to unstage

###### **Options:**

* `--targets <file>` — Path to a targets file



## `lore reset`

Reset changes to a file or directory

**Usage:** `lore reset [OPTIONS] <paths|--targets <file>>`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--purge` — Delete untracked files
* `--targets <file>` — Path to a targets file containing all the paths to all files
* `--revision <revision>` — Revision to reset files to
* `--last-merged-from <branch>` — If given, the files will be reset to the last point of merge from this branch, or the branch point from this branch if no merge has been performed



## `lore diff`

Show differences between two revisions of a file

**Usage:** `lore diff [OPTIONS] [paths]...`

###### **Arguments:**

* `<paths>` — Any number of paths or files

###### **Options:**

* `--source <revision_source>` — Optional signature of the source revision to diff from, by default the current revision
* `--target <revision_target>` — Optional signature of the target revision to diff to, by default the current file system state
* `--diff3` — If given, produce three-way merge output with conflict markers instead of a two-way unified diff
* `-U`, `--context <n>` — Number of unchanged context lines to show around each hunk

  Default value: `3`
* `--ignore-space-at-eol` — Treat lines that differ only in trailing whitespace as unchanged
* `--ignore-space-change` — Collapse runs of internal whitespace to a single space before comparing
* `--targets <file>` — Path to a targets file containing all the paths to all files



## `lore history`

List revisions of a repository

**Usage:** `lore history [OPTIONS] [LENGTH]`

###### **Arguments:**

* `<LENGTH>` — Number of revisions to show

###### **Options:**

* `--revision <revision>` — Start listing from the specified revision. If not specified, start listing from the current branch latest revision
* `--branch <branch>` — Show branch revisions
* `--only-branch` — Stop when reaching a revision on a different branch (includes the branch point revision)
* `--oneline` — Output each revision on one line only



## `lore commit`

Commit the staged revision

**Usage:** `lore commit [OPTIONS] <MESSAGE>`

###### **Arguments:**

* `<MESSAGE>` — Commit message

###### **Options:**

* `--stats` — Print stats
* `--link <LINK>` — Commit only changes in this linked repository (mount path relative to repo root)
* `--link-message <PATH>` — Per-link commit message. Takes two values: <path> <message>. Can be specified multiple times
* `--layer <LAYER>` — Commit only changes in this layer (mount path relative to repo root)
* `--layer-message <PATH>` — Per-layer commit message. Takes two values: <path> <message>. Can be specified multiple times



## `lore sync`

Synchronize to a repository state

**Usage:** `lore sync [OPTIONS] [revision]`

**Command Alias:** `synchronize`

###### **Arguments:**

* `<revision>` — Revision hash signature to synchronize to. Can be a signature on any branch — if the target revision is on a different branch, the current branch is updated accordingly. Can be a partial hash signature

###### **Options:**

* `--forward-changes` — Fast forward any local changes if syncing to a local revision
* `--reset` — Reset any local modified files to match the incoming revision
* `--root-file <path>` — Root files for dependency-based selective sync (only sync changes for these files and their dependencies)
* `--dependency-tag <tag>` — Tags to filter dependencies by during dependency-based sync
* `--dependency-recursive` — Follow transitive dependencies recursively during dependency-based sync
* `--dependency-depth-limit <depth>` — Maximum dependency traversal depth (0 means unlimited)

  Default value: `0`



## `lore push`

Push commits to remote

**Usage:** `lore push [OPTIONS] [branch]`

###### **Arguments:**

* `<branch>` — Optional name or identifier of the branch, push current branch if not specified

###### **Options:**

* `--fast-forward-merge` — Allow the server to fast-forward merge if the target branch head has moved



## `lore lock`

Lock file

**Usage:** `lore lock <COMMAND>`

###### **Subcommands:**

* `acquire` — Acquire lock on file(s)
* `status` — Get lock status on file(s)
* `query` — Query the lock status given a branch, owner or path
* `release` — Release lock on file(s)



## `lore lock acquire`

Acquire lock on file(s)

**Usage:** `lore lock acquire <paths|--branch <branch>>`

###### **Arguments:**

* `<paths>` — Any number of file paths to lock

###### **Options:**

* `--branch <branch>` — Branch where lock is to be acquired



## `lore lock status`

Get lock status on file(s)

**Usage:** `lore lock status [OPTIONS] [paths]...`

###### **Arguments:**

* `<paths>` — Any number of file paths to get the lock status

###### **Options:**

* `--branch <branch>` — Branch where lock was acquired



## `lore lock query`

Query the lock status given a branch, owner or path

**Usage:** `lore lock query [OPTIONS]`

###### **Options:**

* `--branch <branch-name>` — Branch to query locks on
* `--owner <owner-id>` — Owner to query locks belonging to them
* `--path <path>` — Path to query lock information on



## `lore lock release`

Release lock on file(s)

**Usage:** `lore lock release [OPTIONS] [paths]...`

###### **Arguments:**

* `<paths>` — Any number of file paths to release the lock

###### **Options:**

* `--branch <branch>` — Branch where lock was acquired
* `--owner <owner>` — Owner of the lock



## `lore service`

Manage the repository in a service process

**Usage:** `lore service <COMMAND>`

###### **Subcommands:**

* `run` — Run this process as the service
* `start` — Start the service process
* `stop` — Stop the service process
* `set-use-automatically` — Set whether to automatically use the service process



## `lore service run`

Run this process as the service

**Usage:** `lore service run`



## `lore service start`

Start the service process

**Usage:** `lore service start`



## `lore service stop`

Stop the service process

**Usage:** `lore service stop`



## `lore service set-use-automatically`

Set whether to automatically use the service process

**Usage:** `lore service set-use-automatically <enabled>`

###### **Arguments:**

* `<enabled>` — Automatically run Lore commands through the service process

  Possible values: `true`, `false`




## `lore notification`

Notifications

**Usage:** `lore notification <COMMAND>`

###### **Subcommands:**

* `subscribe` — Subscribe to events on the given repository



## `lore notification subscribe`

Subscribe to events on the given repository

**Usage:** `lore notification subscribe [seconds]`

###### **Arguments:**

* `<seconds>` — Time to be subscribed in seconds



## `lore completions`

Generate terminal autocompletions

**Usage:** `lore completions <shell> [path]`

###### **Arguments:**

* `<shell>` — Shell to generate autocompletions for

  Possible values: `bash`, `elvish`, `fish`, `powershell`, `zsh`

* `<path>` — Directory path to write the autocompletion script to



## `lore shared-store`

Manage the shared store

**Usage:** `lore shared-store <COMMAND>`

###### **Subcommands:**

* `create` —
* `info` —
* `set-use-automatically` —



## `lore shared-store create`

**Usage:** `lore shared-store create [OPTIONS] <remote-url>`

###### **Arguments:**

* `<remote-url>` — Remote URL that will back the store

###### **Options:**

* `--path <path>` — Where to create the shared store
* `--make-default <MAKE_DEFAULT>` — Set this as the default shared store in the global config file, defaults to true

  Possible values: `true`, `false`




## `lore shared-store info`

**Usage:** `lore shared-store info`



## `lore shared-store set-use-automatically`

**Usage:** `lore shared-store set-use-automatically <enabled>`

###### **Arguments:**

* `<enabled>` — Whether to automatically use the shared store

  Possible values: `true`, `false`




<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>
