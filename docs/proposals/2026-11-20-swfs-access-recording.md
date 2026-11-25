---
lep: 2026-11-20-swfs-access-recording
title: Access Recording on SWFS
authors:
  - matthew.hoffman
status: Draft
created: 2026-11-20
updated: 2026-11-24
---

# Access Recording on SWFS

## Summary

`lore clone --prefetch <file>` takes an access recording: a list of the paths a workload opened, which a clone fetches in the background so the workload's next run doesn't wait on the network. Today only a FUSE mount on Linux can make a recording. This proposal lets an SWFS mount on Windows make one, in the same format, by sharing the FUSE provider's recorder. A recording made on either provider then prefetches into a clone on the other.

## Motivation

**A Windows user can't capture a workload.** A FUSE mount writes the first access to each path it serves when `LORE_FUSE_RECORD_DIR` is set (`lore-revision/src/fs/fuse/access_recorder.rs`). An SWFS mount has no recorder. A team whose workload runs on Windows, such as a build or an editor session, has to reproduce it on Linux to make a recording, and a workload that only runs on Windows can't be recorded at all.

**A recording is how a clone fetches a known workload ahead of demand.** That matters on Linux today, where a FUSE clone defers file content to first read. It would matter on Windows too if [Align the SWFS Provider with the FUSE Provider](2026-11-19-align-swfs-with-fuse.md) is accepted, which defers an SWFS instance's content the same way and makes `--prefetch` take effect there.

This proposal doesn't depend on that one. A recording made on an SWFS mount prefetches into a FUSE clone as soon as this proposal lands. Prefetching into an SWFS clone waits for the other.

## Goals / Non-Goals

### Goals

1. **G1 — An SWFS mount records.** An SWFS mount with recording enabled writes an access recording of what applications listed and read.
2. **G2 — One format.** The recording has the format a FUSE mount writes, and prefetches into a clone on either provider.
3. **G3 — One recorder.** Both providers share the code that builds and writes a recording, and FUSE's recordings don't change.
4. **G4 — No cost to reads.** Recording adds no wait to the read or listing that made the access.

### Non-Goals

- Revisiting what the FUSE feature decided: the recording's format, the environment variable that enables it, what a mount records, and how `--prefetch` reads it. The [release notes](../release-notes.md) ("FUSE mounts record the first access to each path") and `lore-revision/src/fs/fuse/access_recorder.rs` describe them, and this proposal takes them as they are.
- Making `--prefetch` take effect on SWFS. [Align the SWFS Provider with the FUSE Provider](2026-11-19-align-swfs-with-fuse.md) proposes it.

## Proposed Design

### What an SWFS mount records (G1, G2)

A recording is a text file with one line per path: a file as its repository-relative path, and a directory as its path with a trailing `/` followed by the indices of the tree blocks that hold its children (`lore-revision/src/fs/fuse/filesystem.rs`, `record_dir_access`). Paths are relative to the repository and block indices belong to the revision, so neither depends on the provider.

An SWFS mount with recording enabled writes the first access to each path it serves from the revision:

- **A file** when a read of it returns bytes.
- **A directory** when Lore lists it for SWFS.
- **Ancestors.** A file read records the directories above it that the recording lacks, so the recording carries the lines a prefetch needs to reach the file whether or not Lore listed those directories for SWFS first.
- **Links and layers.** The mount records a path below a link or layer relative to the root of the mounted working tree, as it records any other path: the link's or layer's mount path, followed by the path inside the linked or layered repository. A file at `src/main.c` in a repository linked at `libs/foo` is recorded as `libs/foo/src/main.c`. The mount records a directory there without block indices, because those would index the linked or layered repository's revision, not the parent's. The reader accepts a directory line with no indices (`parse_recording` in `lore-revision/src/repository/prefetch.rs`), so the format doesn't change. A prefetch resolves such a path through the mount it prefetches into. Where that mount doesn't serve the path, as a FUSE mount doesn't serve a link, the prefetch fetches nothing for it and carries on with the rest of the recording.
- **Not the user's changes.** The mount doesn't record a path whose content the user's changes hold, since the revision has nothing to prefetch for it.

### One recorder for both providers (G3)

- **Shared.** The writer that appends lines to the recording file, and the code that builds a line, become code both providers build. They live in the FUSE module today, which builds only on Linux.
- **Per provider.** Each mount reports the accesses it answers. Each also remembers which paths it has recorded, so that both record a path once, as FUSE does today.
- **FUSE is unchanged.** The move preserves what a FUSE mount records, and FUSE's recording tests check it.

### No cost to reads (G4)

A callback hands its line to the writer and returns. The writer appends in the background and flushes at an interval and at unmount, as it does for FUSE (`access_recorder.rs`). Lines wait in memory until the writer appends them. Each path is recorded once, so what can wait is bounded by the number of distinct paths accessed. If the file can't be written, the mount drops the lines and the read succeeds.

### Turning it on

`LORE_FUSE_RECORD_DIR` in the service's environment enables recording on an SWFS mount, as it does on a FUSE mount. Each mount writes its own file in that directory, `<instance id>-<unix seconds>.txt`, as the release note describes.

## Compatibility

- **Wire format** — N/A. No message changes.
- **Client/server protocols** — N/A. A recording is a local file.
- **On-disk format** — No repository or instance format changes. The recording file keeps FUSE's format, so an earlier Lore's `--prefetch` reads a recording an SWFS mount made.
- **CLI and public API** — No subcommand, option, exit code or `lore-capi` surface changes.
- **Environment variables** — `LORE_FUSE_RECORD_DIR` gains an effect on Windows, where it had none. A service that already has it set starts writing recordings for its SWFS mounts. The variable's name and meaning are FUSE's and don't change.

## Non-Functional Considerations

- **Concurrency** — Callbacks on any number of threads hand lines to one writer per mount. The order of lines follows the order the writer receives them, which is the order of first access as closely as concurrent callbacks define one.
- **Memory** — A mount with recording enabled remembers the paths it has recorded and holds the lines its writer hasn't appended yet. Both grow with the number of distinct paths applications access, not with the size of the repository, and both are zero with recording off.
- **Statelessness** — A mount with recording enabled keeps its writer and its memory of recorded paths in the service. Both end with the mount.
- **Determinism** — Preserved for everything Lore serves. A recording itself depends on what applications did and in which order.
- **Latency** — Preserved. A read or listing hands its line to the writer and doesn't wait for the file write (*No cost to reads*).

## Migration Plan

N/A — no breaking changes, no migration required.

## Security Considerations

This proposal adds no security surface beyond the one FUSE recording has. A recording holds paths and block indices, no content and no credentials. The service writes it to the directory its own environment names, with the service's access, so a user who can't set the service's environment can't make it write anywhere. An SWFS recording is the same file, written the same way, by the same shared code. Reading a recording is `--prefetch`'s concern and is unchanged.

## Privacy Considerations

A recording lists the repository paths a user's applications opened, in the order they opened them. That is as true of a FUSE recording today. This proposal extends it to Windows users and changes nothing else: Lore writes a recording only when the operator sets the recording directory, to that directory, and sends it nowhere. The operator deletes it as any other file, and Lore keeps no copy.

## Risks and Assumptions

**Assumptions**

- **Assumption:** Lore answers the first read of each file an SWFS mount serves from the revision, so the mount sees every first access to revision content. — *invalidated if:* a first read can be answered without Lore, which would leave files out of the recording.
- **Assumption:** A recording's lines mean the same on both providers for paths in the instance's own repository. — *invalidated if:* the two providers spell a path, or number a directory's tree blocks, differently for the same revision.

**Risks**

- **Risk:** Moving the recorder out of the FUSE module changes what FUSE records. — *mitigation:* the move is behavior-preserving and lands alone, and FUSE's recording tests check it.
- **Risk:** An SWFS recording lacks the directory lines a prefetch needs to reach a file. — *mitigation:* a file read records its missing ancestors, and a test prefetches a FUSE clone from a recording an SWFS mount made, kept with the tests because the two providers run on different platforms, and checks the recorded files are local.

## Drawbacks

- Windows recording takes its switch from an environment variable named for FUSE.
- An SWFS mount with recording enabled spends memory on every distinct path accessed, for as long as it is mounted.

## Alternatives Considered

### An SWFS recorder of its own

SWFS gets a recorder written in the SWFS module, and FUSE's stays where it is.

*Rejected because:* two recorders can differ in the lines they write, and a recording made on one provider might then not prefetch on the other. The line format and the writer don't depend on the provider.

### Record from SWFS's own event log

SWFS hands its client an event log to store (`write_event` in `lore-revision/native/swfs/swfs.h`). Lore could derive a recording from it after the fact.

*Rejected because:* the log's bytes are SWFS's to interpret, so Lore would depend on a format the contract doesn't describe. A recording also needs the tree block indices a directory line carries, which only Lore knows, and Lore would have to resolve every path again against a revision that may have moved.

### Derive the recording from what is local (status quo plus a tool)

A tool lists the files an instance has fetched and writes them as a recording.

*Rejected because:* what is local includes everything any earlier prefetch or read fetched, not one workload, and it has no order. A prefetch fetches in recorded order so that what a workload opens first arrives first.

## Prior Art

- **Lore's FUSE provider** (`lore-revision/src/fs/fuse/access_recorder.rs`) is the model, and this proposal shares its recorder.
- **VFS for Git** ([microsoft/VFSForGit](https://github.com/microsoft/VFSForGit)) offers a prefetch command that takes the files and folders a user lists. A recording spares the user from working out that list.

## Unresolved Questions

None. Questions about recording itself, such as the variable's name or what a recording should leave out, belong to the feature as FUSE has it and are out of scope here (*Non-Goals*).
