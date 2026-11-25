---
lep: 2026-11-19-align-swfs-with-fuse
title: Align the SWFS Provider with the FUSE Provider
authors:
  - matthew.hoffman
status: Draft
created: 2026-11-19
updated: 2026-11-24
---

# Align the SWFS Provider with the FUSE Provider

## Summary

Lore has two virtual file system providers: FUSE on Linux and SWFS on Windows. Both present a repository instance's working tree through a mount, without writing its files to disk first, but they take a revision in different ways. The FUSE provider holds the revision it serves as a `State`, so a clone or sync hands it the revision whole, and the tree and the file content arrive from the remote when something reads them. The SWFS provider is given the revision one file at a time, and a clone or sync downloads the whole tree and every file's content before it returns. This proposal makes the SWFS provider hold its state as the FUSE provider does. An SWFS clone or sync then defers the download of the revision tree and of file content, the mount answers reads concurrently, and `lore clone --prefetch <file>` prefetches into an SWFS instance. Where the FUSE provider already has the machinery, SWFS shares it instead of gaining a copy. Two things an SWFS instance does today stop: it is no longer fully readable offline straight after a clone, and a clone no longer accepts `--root-file`.

## Motivation

**An SWFS clone or sync pays for the whole revision before it returns.** [SWFS Support Through the FilesystemProvider Interface](2026-10-29-swfs-through-the-filesystem-provider.md) routes an SWFS clone and sync through the path an on-disk instance uses: Lore walks the revision's tree and tells the provider about each file. That walk reads every node of the tree the view filter selects, and it fetches each file's content whole into the local immutable store (`set_file_to_immutable_store_contents` in `lore-revision/src/fs/swfs/filesystem.rs`). The cost grows with the size of the repository, which is the cost a virtual file system exists to remove. Lore's default model is lazy fetch, where a client pulls only what it needs, on demand ([glossary](../glossary.md), *Lazy fetch*). An SWFS instance doesn't follow it, although its mount can already fetch from the remote what its store lacks ([release note](../release-notes/swfs-fetches-what-its-store-evicted.md)).

**The two virtual providers behave differently for the same command.** A FUSE clone returns once the provider holds the cloned revision, and fetches what a read needs afterwards (`clone_onto_held_state` in `lore-revision/src/repository/clone.rs`). A team with Linux and Windows users gets a different clone time, a different sync time and different offline behavior from `--vfs fuse` and `--vfs swfs`, for reasons that have nothing to do with the platform.

**The providers reach the commands through different interfaces.** `FilesystemProvider::holds_state` (`lore-revision/src/fs/filesystem_provider.rs`) splits clone, sync and commit into two paths: one for a provider that holds its state, which FUSE takes, and one for a provider over files, which the on-disk provider and SWFS take. SWFS is a virtual provider on the path built for files on disk. Each improvement to the path for virtual providers reaches FUSE alone, and SWFS needs separate work to follow.

**SWFS runs a mount's callbacks one at a time.** SWFS makes its callbacks on the mount's one work thread (`lore-revision/native/swfs/swfs.h`, on `swfsFreeze` and `swfsThaw`: a call "made on swfs's own work thread (a callback)" can't wait). While Lore answers reads from a local store, serial callbacks cost little. Once a read can wait on the remote, one slow read holds every other read and listing on the mount.

**`--prefetch` does nothing on SWFS.** `lore clone --prefetch <file>` names an access recording whose paths a FUSE instance fetches in the background ([CLI reference](../reference/lore-cli-commands.md)). An SWFS clone accepts the option and ignores it: only the clone path for a provider that holds its state reads the recording, and only the FUSE provider starts the background task (`InstanceOperationImpl::start_prefetch` in `lore-revision/src/fs/filesystem_provider.rs`).

## Goals / Non-Goals

### Goals

1. **G1 — SWFS holds its state.** The SWFS provider answers `holds_state` as the FUSE provider does, and clone, sync and commit reach both through the same path.
2. **G2 — Deferred revision tree.** An SWFS clone or sync returns without downloading the revision's tree. The mount loads the part of the tree a listing or read needs when it needs it.
3. **G3 — Deferred file content.** An SWFS clone or sync downloads no file content that a FUSE one wouldn't. The mount fetches a file's content when an application reads it.
4. **G4 — Concurrent reads.** An SWFS mount answers several read callbacks at once, up to a number of threads Lore sets, so a file read or directory listing that waits on the remote doesn't delay another.
5. **G5 — `--prefetch` on SWFS.** `lore clone --prefetch <file>` prefetches into an SWFS instance as into a FUSE one, through the same background task.
6. **G6 — No change to FUSE.** Code that moves out of the FUSE provider to be shared keeps FUSE's behavior.

### Non-Goals

- Fixing defects in the SWFS library. This proposal designs to the contract SWFS documents and is expected to keep (`lore-revision/native/swfs/swfs.h`), not to how a given build behaves. Where a build falls short of the contract, SWFS fixes it.
- Changing what the FUSE provider does. This proposal takes FUSE's behavior as the one that meets users' needs, and doesn't revisit it.
- An option that keeps the full download on SWFS. FUSE has none, and a recording passed to `--prefetch` is how either provider fetches a known workload ahead of demand.
- Improving the background prefetch, such as a completion signal a script can wait on. This proposal brings the prefetch FUSE has to SWFS as it is.
- Making an access recording on an SWFS mount. [Access Recording on SWFS](2026-11-20-swfs-access-recording.md) proposes it.
- Sequential read-ahead on SWFS. It refines deferred reads and can be proposed on its own.
- Starting a background prefetch on sync or remount. FUSE starts one only from the clone path (`clone_onto_held_state`).
- Other virtual file systems, and running SWFS mounts outside the Lore service.

## Proposed Design

The design has one structural change, from which most of the rest follows: the SWFS provider holds its state. Deferred tree and content, and `--prefetch`, are what the path for a provider that holds its state already does. Concurrent reads are an addition to the SWFS mount.

What doesn't change: the `FilesystemProvider` seam, the freeze and thaw around each operation, the modified-file record that tells Lore what the user changed, the rule that Lore describes a revision's changes to the thaw instead of writing them through the mount, and the service-only mount lifecycle. The accepted proposal describes each of these.

This proposal calls the background task that fetches ahead of demand the *background prefetch*. The glossary's *Prefetch* is the part of it that fetches a recording's files. The task also loads the revision's tree, which FUSE does on every clone.

### SWFS holds its state (G1)

A provider that holds its state serves the working tree from a `State`, the revision with the user's local changes, and not from files. Three hooks carry a revision to such a provider (`lore-revision/src/fs/filesystem_provider.rs`, `lore-revision/src/fs/hold.rs`):

- **Clone and sync hand over the target.** The command gives the provider the target revision's `State` to hold. It doesn't walk the tree and makes no per-file call.
- **Commit hands over what it stored.** The commit tells the provider which content it stored for each file it read, then hands it the committed revision.
- **Change detection needs no tree walk.** The provider answers which paths the user changed from what it holds.

The SWFS provider takes all three. At a high level, the SWFS code changes as follows:

- **The served tree is the held state.** An SWFS mount already serves from a `State` it builds for the current revision (`lore-revision/src/fs/swfs/served_tree.rs`). That `State` becomes the one the provider holds, and a hand-over replaces it. The mount's callbacks, an operation's reads and the thaw keep resolving paths as they do now.
- **A hand-over is a change list, described to the thaw.** When a sync hands over a target, the provider takes the changes between the revision it holds and the target, checks them against the modified-file record, and names the changed paths in the thaw that ends the operation. The work on the mount follows the size of the change, not the size of the tree. On a clone the mount is empty, so the thaw names the root alone, and SWFS asks Lore to list each directory when something opens it.
- **Local changes are refused, kept, overwritten or reset as on FUSE.** A hand-over that would replace a user's change refuses, unless the sync asks to keep, overwrite or reset local changes. The check and the report of what the hold changed are the shared ones in `lore-revision/src/fs/hold.rs`, so both providers refuse the same syncs and report the same changes. What differs is where the user's content lives: FUSE keeps it in its write directory, and SWFS keeps it in SWFS, where a thaw that names a path replaces it and one that doesn't leaves it.
- **A failed hand-over leaves the working tree as an on-disk instance's would be.** The working tree an SWFS instance shows after an operation must match what an on-disk instance holds after the same operation, including one that fails partway ([ADR-00034](../developing/decisions/00034-swfs-serve-views-and-partial-trees.md)). A path whose change the operation didn't apply keeps what the working tree held, and the user's changes are untouched.
- **Commit uses the shared hooks.** The provider learns what a commit stored through the same hooks FUSE uses, and names those paths in the thaw so SWFS drops its record of the change. The SWFS-only call that names committed paths goes away.
- **Per-file writes remain for the user's edits only.** Lore still writes through the mount what is an edit on the user's behalf and not part of the revision: a merge or resolve result, and the scratch copies a conflicted merge leaves ([ADR-00033](../developing/decisions/00033-swfs-realize-operation-results-the-revision-does-not-hold.md)). A clone or sync no longer reaches the provider through per-file writes.

**Links and layers.** Lore refuses several things on a provider that holds its state: adding a link, syncing layers, and committing a link alone or a layer. Those refusals describe what the FUSE mount can serve: it serves one `State` of one repository, and fails a listing that reaches a link (`lore-revision/src/fs/fuse/listing.rs`, "a FUSE mount of a revision holding a link"). They don't follow from holding a state. A revision records its links in a list of their own, apart from its tree (`State::link_list` in `lore-revision/src/state.rs`), so a provider finds them without walking the tree, and the SWFS mount already serves links and layers from their own repositories that way ([ADR-00036](../developing/decisions/00036-swfs-serve-links-and-layers.md)). That stays. Under this proposal the refusals follow what a provider can serve, not whether it holds its state, and SWFS keeps every command it supports today.

One part of the hand-over does need to grow. It carries one repository's `State`, and a link's or layer's files belong to another repository. A sync that moves a link's pin shows in the parent's change list as a change to the link itself, and the SWFS provider names the link's mount path in the thaw, which relists what is below it ([ADR-00037](../developing/decisions/00037-swfs-describe-link-and-layer-changes-by-mount-path.md)). A layer isn't part of the parent's revision at all, so a sync of layers and a commit of a link alone or of a layer tell the provider which mounted repository moved, and the provider reads that repository's new revision as it does today.

**View filter.** An SWFS mount honors the instance's view filter today: it leaves every path the view excludes out of its listings and reads, with one check per listed entry ([ADR-00034](../developing/decisions/00034-swfs-serve-views-and-partial-trees.md)). A FUSE mount does the same when it builds a listing (`lore-revision/src/fs/fuse/view.rs`). The check needs the path and the filter and nothing from the clone, so it stays as it is when SWFS holds its state.

**Selective sync.** A selective sync limits a clone or a sync to chosen root files and the files they depend on ([glossary](../glossary.md), *Selective sync*). This proposal writes *selective clone* for the clone case. Both providers refuse a sync limited that way today, each for its own reason (`sync` in `lore-revision/src/revision/sync.rs`). FUSE holds whole revisions. On SWFS the working tree matched an on-disk instance's, but a scan afterwards reported the kept files differently, so a user could stage or commit the wrong paths ([ADR-00034](../developing/decisions/00034-swfs-serve-views-and-partial-trees.md)). This proposal doesn't change that. A provider that holds its state refuses a selective clone (`refuse_on_held_state` in `lore-revision/src/repository/clone.rs`). SWFS accepts one today, and under this proposal it refuses one, as FUSE does. A provider that holds its state presents a whole revision, and once content is deferred a selection no longer limits a download, so a selective clone has nothing left to do there. A user who wants part of a revision presented uses a view filter, and one who wants a known set of files local uses `--prefetch`. The set of absent paths that lets SWFS accept a selective clone today (that ADR) stays for the other thing it covers, the changes a failed operation leaves unwritten.

**How this relates to the accepted proposal.** This proposal refines the accepted one and doesn't replace it. The accepted proposal's goals, seam and staging stand. Its original text had SWFS take whole revisions through the hold hooks, and its 2026-11-02 amendment records that the implementation took the per-file path instead ([SWFS through the filesystem provider](2026-10-29-swfs-through-the-filesystem-provider.md), *Amendments*, "No held-`State` hand-over"). This proposal returns SWFS to the route that proposal first described, because the per-file path is what downloads the whole revision. Accepting this proposal therefore commits to two changes to the record. Both land with the implementation, not with this proposal:

- An amendment to the accepted proposal. Until the implementation lands, that proposal keeps describing what is built, which is the per-file path. The amendment then rewrites its text wherever it describes that path for clone, sync and commit. Its account of links and layers and of the remaining commands stands.
- An ADR that supersedes one decision driver of [ADR-00042](../developing/decisions/00042-swfs-store-link-and-layer-changes-inside-their-operation.md): that the provider interface should not carry a state hand-over for SWFS. The new ADR decides that both virtual providers take the hand-over FUSE already takes. ADR-00042's outcome stands: a command still stores a link or layer change inside the operation that makes it, and the thaw still describes it. The ADR lands with the change that moves SWFS onto the hand-over, as a decision record follows the code that makes it concrete.

### Deferred revision tree (G2)

A `State` doesn't need its tree in memory, or in the local store, to be held. It loads a block of the tree when a lookup reaches it (`State::node` in `lore-revision/src/state.rs`), from the local store, or from the remote when the store lacks it, which is how a FUSE mount lists a directory after a clone that walked nothing (`lore-revision/src/fs/fuse/listing.rs`). Holding a `State` therefore defers the tree without a change to how the SWFS provider is structured: the callbacks that list a directory or resolve a path already read through a `State`, and now that read may load a block.

- **Clone.** A clone caches the revision's own records, hands the `State` to the provider and returns. It doesn't walk the tree.
- **Sync.** A sync reads the parts of the tree where the held revision and the target differ, to build its change list, and no more.
- **After the command.** A directory listing or a file lookup loads the blocks it needs. The background prefetch loads the rest of the tree, as on FUSE (*`--prefetch` on SWFS*), so the user doesn't pay for a first listing where the network allows.
- **Offline.** A listing whose blocks aren't local fails while the remote is unreachable. `lore clone --offline` prepares an instance for staging and committing without the remote, as it does on FUSE (`clone_onto_held_state`).

### Deferred file content (G3)

A clone or sync fetches no file content on the mount's behalf, except what a FUSE one fetches: the revision's root ignore file, so that a command run offline finds it (`clone_onto_held_state`). The mount's read callback reads the requested range through the store, and fetches a range the store lacks from the remote and keeps it. An SWFS mount already connects to its remote and fetches what its store has evicted ([release note](../release-notes/swfs-fetches-what-its-store-evicted.md)). Under this proposal that fetch becomes how most content first arrives, where today it is the exception. This has several consequences, each of which matches FUSE:

- **Reads depend on the remote.** The mount's connection carries every first read of a file, not only reads of evicted content. It connects as the caller that mounted the instance, as today.
- **A read can fail.** A read whose content can't be fetched, because the remote is unreachable, refuses the read, or returns content that fails its check, fails the application's read with an I/O error. SWFS's contract for a callback that returns fewer bytes than the file holds is a failed read (`SWFSResult_VirtualFileReadFailed` in `swfs.h`). Lore never answers a read with bytes that aren't the revision's.
- **A read is bounded.** A read that has to fetch gives up after a deadline and fails, as a FUSE read does (`FETCH_DEADLINE` in `lore-revision/src/fs/fuse/read.rs`), so an application doesn't wait without limit on a remote that has stopped answering.
- **Reads are verified.** The mount checks the content it fetches as [ADR-00023](../developing/decisions/00023-data-verification-and-trust.md) requires of remote content.
- **Offline use changes.** An SWFS instance cloned before this proposal holds every file locally, apart from what its store has since evicted, and one cloned after holds what an application has read or a prefetch has fetched. Offline, a read of anything else fails.
- **Writes on the user's behalf still fetch.** Lore fetches the content it writes through the mount as bytes, such as a merge result, before the operation ends, because SWFS serves those bytes from its own write directory.
- **Commands fetch no more than today.** Status, stage and commit read only what the modified-file record names ([ADR-00031](../developing/decisions/00031-swfs-modified-file-record.md)), so deferring content adds no fetches to them.

### Concurrent reads (G4)

**This proposal depends on an SWFS feature.** SWFS calls a mount's client through reader callbacks and writer callbacks. A reader callback serves both a file's content and a directory's listing, so what this section says of reads holds for listings too. `swfs.h` describes one work thread for a mount's callbacks today. A future SWFS version lets a client set, when it mounts, how many threads SWFS runs the mount's read callbacks on. This proposal depends on that feature, and Lore asks for more than one read callback thread on every mount.

The number of threads bounds how many reads can wait on the remote at once. A read past that number waits in SWFS for a thread, for no longer than the read deadline of the reads ahead of it (*Deferred file content*).

Lore answers callbacks on as many threads as SWFS calls them on. The design guarantees:

- **No mount-wide wait.** A callback holds no lock shared with other callbacks while it reads the store or waits on the remote. A read that waits on the remote delays no other read or listing on the mount.
- **A consistent snapshot per callback.** Each callback resolves its path against one published `State`. A hand-over publishes the new `State` in one step, so a callback sees the revision before the operation or after it, and never part of each.
- **One fetch per fragment.** Callbacks that need the same content at once share one fetch.
- **Priority over background work.** A fetch an application waits on goes ahead of the background prefetch's fetches on the same connection.
- **Correct at any thread count.** Nothing in Lore depends on how many callbacks arrive at once. The thread count changes how many reads proceed together, not what they return.

Freeze and thaw keep the ordering SWFS's contract gives them against callbacks in flight. Lore adds no ordering of its own between a callback and an operation, beyond publishing the `State` in one step.

### `--prefetch` on SWFS (G5)

Once the SWFS provider holds its state, an SWFS clone takes the clone path that reads the recording and starts the background prefetch. The option needs no SWFS-specific handling in the command. The task runs after the clone returns, and has two parts: the recording's paths, and the tree. Unmounting cancels it, and a later hand-over stops it from starting further work, as on FUSE (`lore-revision/src/fs/fuse/prefetch.rs`).

Most of the machinery is reusable, in three groups:

- **Shared already.** The plan, the recording reader with its size limit, the choice of how much of the tree to load (`LORE_TREE_PREFETCH_DEPTH`), and the fetch of the tree's blocks are provider-neutral today (`lore-revision/src/repository/prefetch.rs`). So is the clone path that builds the plan and starts the task. SWFS uses them as they are.
- **Shared after a move.** The task itself lives in the FUSE module, which builds only on Linux. What it does is provider-neutral: it fetches the blocks the recording names, resolves each recorded path in recorded order, fetches each recorded file's content, limits how much is in flight, logs a summary and stops once superseded. Both providers run that one task, so a recording produces the same fetches on either.
- **Per provider.** Two things depend on the mount. Resolving a recorded path to the file or directory the mount serves: FUSE resolves through its lookups, and SWFS through the tree it serves, which also covers a path below a link or layer. And telling the task that a later hand-over has superseded it.

This proposal requires of the shared task what *Concurrent reads* promises: its fetches share the connection's limits with other background fetches and go behind the reads applications wait on.

### Keeping FUSE as it is (G6)

Each move of FUSE code into shared code preserves FUSE's behavior and lands before the SWFS change that needs it. FUSE's prefetch tests are the oracle. The command tests run against a virtual instance where `LORE_TEST_VFS` names one (`scripts/test/lore.py`), so they check the commands SWFS supports today across the change of path.

## Compatibility

- **Wire format** — N/A. No message changes.
- **Client/server protocols** — N/A. The mount fetches through the storage protocol every read uses. A server sees more of an SWFS instance's reads arrive after the clone, and sees no new request.
- **On-disk format** — No format changes. An upgraded Lore serves an existing SWFS instance, whose content is already local. A downgraded Lore serves an instance cloned or synced after the upgrade: its mount fetches from the remote what its store lacks, provided it is recent enough to do so ([release note](../release-notes/swfs-fetches-what-its-store-evicted.md)). A Lore older than that serves only what is local, and a read of content that was never fetched fails there.
- **CLI and public API** —
  - `lore clone --vfs swfs` and `lore sync` on an SWFS instance return without the tree or the file content local. A script that relied on an SWFS instance being readable offline straight after a clone no longer can.
  - `lore clone --prefetch <file>` takes effect on an SWFS instance, where it was ignored. Its help text says "virtual instance" in place of "FUSE instance".
  - `lore sync` on an SWFS instance with local changes refuses, keeps, overwrites or resets them by the rules a FUSE instance follows (`lore-revision/src/fs/hold.rs`). The two sets of rules are meant to agree, and this proposal knows of no sync that one accepts and the other refuses. The command tests run before and after the change of path, and a difference they show is a compatibility change to record here.
  - `lore clone --vfs swfs --root-file` fails with the refusal a FUSE clone gives, where it succeeded. This is a breaking change (*Migration Plan*).
  - `LORE_TREE_PREFETCH_DEPTH` applies to an SWFS clone.
  - No subcommand, option, exit code or `lore-capi` surface is added or removed.
- **SWFS library** — Lore relies on two parts of the contract in `swfs.h`: a callback that returns fewer bytes than the file holds fails the application's read, and a thaw that names a directory relists it against Lore. Lore also depends on a future SWFS version that lets a client set the number of read callback threads for a mount, which `swfs.h` doesn't offer today (*Concurrent reads*).

## Non-Functional Considerations

- **Concurrency** — Read callbacks run concurrently with each other, up to the number of threads Lore sets for the mount, and with the background prefetch and commands against the instance (*Concurrent reads*). One command at a time holds an operation on a mount, as today ([ADR-00026](../developing/decisions/00026-concurrent-read-only-instance-operations.md)). A hand-over publishes in one step.
- **Memory** — Improves on the tree: a `State` loads the blocks a lookup reaches, where a clone today walks the whole view. A read fetches the range asked for, not the file. The background prefetch bounds what it has in flight. Nothing is buffered in proportion to repository or file size.
- **Statelessness** — The service keeps, for each mount, a connection to the remote, as today, and at most one background prefetch. The task lives in memory in the service, ends with the mount, and matches what a FUSE mount keeps. A restart loses a prefetch in progress and doesn't start another.
- **Determinism** — Preserved. The bytes a read returns depend only on the revision and the user's changes. Deferring a fetch changes when the bytes arrive and whether a read can fail, not what they are. A prefetch changes only what is local.
- **Latency** — Changes. A clone or sync returns in time that follows the size of the change, not of the repository. The cost moves to first access: the first listing of a directory and the first read of a file wait on the remote, bounded by the read deadline. A prefetch pays that cost ahead of the user for the paths a recording names.
- **Observability** — This proposal adds a log line for each failed read, with its path and cause. `lore service status` lists a mount's connection error, as today. The background prefetch logs one summary of what it fetched, skipped and failed, as on FUSE.

## Migration Plan

No format or protocol changes, so there are no transition phases. The change ships in the order *Keeping FUSE as it is* gives: the behavior-preserving moves of FUSE code first, then the SWFS provider's change of path. A release note states that an SWFS clone and sync no longer download file content and that offline reads need content read or prefetched beforehand.

Two behaviors break. The first is a script that clones an SWFS instance and then reads it offline. Its replacement is a clone with `--prefetch <file>`, where the recording names what the script reads. The clone returns before the prefetch finishes, and the prefetch gives a script no completion signal other than its summary in the service log (*Drawbacks*). A script that needs every file local has no replacement on a virtual instance, as on FUSE, and clones an on-disk instance.

The second is `lore clone --vfs swfs --root-file`, which starts to fail. A selective clone gave an SWFS instance two things: fewer files downloaded, and fewer files in the working tree. Deferred content replaces the first without any option. A view filter replaces the second where path patterns can describe the set. Where only the dependency set can, the replacement is a selective clone of an on-disk instance. An SWFS instance that a selective clone made before the upgrade keeps working, and keeps the paths it left absent.

Rollback is a downgrade of Lore. A downgraded mount serves an instance cloned or synced under this proposal, and fetches on read what the store lacks, provided the earlier version is recent enough to fetch (*Compatibility*). The signal for a rollback is reads that fail or stall on mounts whose remote is reachable. No state needs recovery, because this proposal writes nothing an earlier version can't read.

## Security Considerations

- **Trust model.** Unchanged. The mount already fetches remote content while no command runs, and verifies it as a command's read does (ADR-00023), so a remote can't make a mount serve bytes that don't match the revision. This proposal makes that fetch common, not new.
- **Credentials.** Unchanged. The mount's connection outlives the command that mounted it, and reads as that caller for as long as the instance is mounted, as a FUSE mount's does (`lore-revision/src/fs/fuse/mount_manager.rs`). A user whose access to the remote is revoked keeps reading what is already local, and fetches nothing new once the remote refuses the connection. With less content local, revocation takes effect on more of the instance than it does today.
- **A crafted recording.** A recording passed to `--prefetch` can only name paths. The background prefetch fetches a path only where the revision holds it and the view filter includes it, with the caller's own access, so a recording grants nothing the user couldn't read. The reader's size limit bounds what a recording costs to parse.
- **A crafted repository.** A revision with a very large tree no longer costs a clone a walk of it. The mount loads what is read.

## Privacy Considerations

- **The server sees reads as they happen.** With content deferred, a server operator can see which fragments an instance fetched and when, which follows what the user opened. The server already sees this for a FUSE instance and for any lazy fetch.
- **No new data is written or sent.** This proposal adds no file, log field or message that carries user data. A recording passed to `--prefetch` is read locally and sent nowhere.
- **Deletion and expiry** are unaffected.

## Risks and Assumptions

**Assumptions**

- **Assumption:** SWFS fails an application's read when a callback returns fewer bytes than the file holds, as `swfs.h` documents. — *invalidated if:* SWFS can't report a failed read to the application, which would leave Lore no correct answer for a read it can't fetch.
- **Assumption:** A future SWFS version runs a mount's read callbacks on the number of threads the client sets. — *invalidated if:* SWFS doesn't gain the setting, or runs read callbacks one at a time whatever the setting. G4 then can't be met, and a mount whose reads wait on the remote stalls behind each one.
- **Assumption:** A thaw that names a directory makes SWFS relist it against Lore, so a clone or sync needn't name every path below it. — *invalidated if:* a thaw has to name each file for SWFS to serve it, which would bring back a walk of the tree.
- **Assumption:** A `State` can be held, listed and diffed with only part of its tree local. — *invalidated if:* an SWFS path, such as serving links and layers, needs the whole tree in the store.
- **Assumption:** FUSE's behavior for clone, sync, reads and prefetch meets users' needs on Windows as on Linux. — *invalidated if:* a Windows workload needs every file local before it starts and a recording can't supply that.

**Risks**

- **Risk:** Taking the path for a provider that holds its state brings its refusals of links and layers to SWFS. — *mitigation:* the refusals move to follow what a provider can serve before SWFS changes path, and the link and layer command tests run against SWFS before and after.
- **Risk:** Moving the background prefetch out of the FUSE module changes FUSE's behavior. — *mitigation:* each move is behavior-preserving, lands alone, and is checked by FUSE's existing prefetch tests.
- **Risk:** A mount whose connection to the remote is lost keeps failing reads after the network returns, and with content deferred that fails most reads. — *mitigation:* the mount connects again at the next command, as it does today, and `lore service status` lists the error meanwhile.
- **Risk:** An application handles a failed read badly, where it never saw one while every file was local. — *mitigation:* a read fails only after the deadline or a refusal from the remote, the mount logs its path and cause, and a prefetch makes the files a workload needs local beforehand.
- **Risk:** Every read callback thread waits on the remote at once, and further reads on the mount wait behind them. — *mitigation:* the read deadline bounds each wait, the thread count is Lore's to raise, and a prefetch makes a workload's files local so its reads don't wait at all.
- **Risk:** A background prefetch competes with the reads a user is waiting on. — *mitigation:* background fetches run under a shared limit, and a waiting read goes ahead of them (*Concurrent reads*).

## Drawbacks

- An SWFS instance is no longer fully readable offline after a clone.
- An application's first read of a file waits on the network.
- A script can't tell when a background prefetch has finished, short of reading the service log.
- An SWFS clone no longer accepts `--root-file`.

## Alternatives Considered

### Keep the per-file path and stop its downloads

SWFS stays on the path for a provider over files, and its per-file calls stop fetching content. Reads fetch on demand as in this proposal.

*Rejected because:* it defers content and nothing else. A clone still walks the whole tree and names every path to the thaw, so its cost still grows with the repository. `--prefetch` still needs separate SWFS handling, because the option is read on the other path. And the two virtual providers stay on different paths, which is the cause of the differences this proposal removes.

### An SWFS background prefetch of its own

SWFS gets its own background task, written in the SWFS module, and FUSE's code stays where it is.

*Rejected because:* two tasks can differ in what they fetch, in what order and under which limits, so one recording would produce different fetches on each provider. The parts that differ by provider are small, and the rest is cheaper to share than to keep in step.

### Keep accepting a selective clone on SWFS

SWFS keeps `--root-file` on clone, either by prefetching the dependency set or by keeping the set of absent paths and filling it at clone.

*Rejected because:* prefetching the set presents the whole revision, which changes what `--root-file` means on SWFS alone. Filling the set of absent paths keeps today's meaning, but it resolves the dependency set at clone, which is the walk a deferred tree removes, and it leaves the two virtual providers different for the same command.

### Serialize callbacks in Lore

Lore answers one callback at a time for each mount, whatever SWFS does, and keeps the simpler single-threaded mount.

*Rejected because:* with content deferred, one read that waits on the remote would hold every other read and listing on the mount for as long as it waits. That limit would then be Lore's and would remain whatever number of read callback threads SWFS offers.

### Keep the full download on SWFS (status quo)

SWFS keeps downloading the tree and all content at clone and sync, and gains only `--prefetch`.

*Rejected because:* a prefetch has nothing to fetch on an instance that already downloaded everything, and the cost that grows with the repository stays.

## Prior Art

- **Lore's FUSE provider** (`lore-revision/src/fs/fuse/`) is the model. This proposal borrows its hand-over of a held `State`, its on-demand reads and its background prefetch.
- **VFS for Git** ([microsoft/VFSForGit](https://github.com/microsoft/VFSForGit)) virtualizes a Git working tree on Windows. It fetches a file's object when an application first reads it, not at checkout, and offers a prefetch command to download ahead of demand. It runs on the Projected File System, whose callbacks a provider may answer asynchronously and may fail with an error ([ProjFS programming guide](https://learn.microsoft.com/en-us/windows/win32/projfs/projfs-programming-guide)), which are the two properties this proposal expects of SWFS.
- **EdenFS** in Sapling ([overview](https://github.com/facebook/sapling/blob/main/eden/fs/docs/Overview.md)) loads trees and file content on first access, and a checkout updates the tree it presents without fetching file content first.

## Unresolved Questions

None. An SWFS read takes the deadline a FUSE read has (*Deferred file content*).
