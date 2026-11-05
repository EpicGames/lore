---
lep: 2026-11-03-instance-ignore-rules
title: Instance ignore rules
authors:
  - Zack Neyland
status: Draft
created: 2026-11-03
updated: 2026-11-05
discussion: https://web.lore.internal.epicgames.net/epic/Lore/change-request/1111
---

# Instance ignore rules

## Summary

A repository's tracked `.loreignore` holds ignore rules every instance of the repository shares. This
proposal adds an `ignore` file in the instance's Lore directory (`.lore/ignore` for a regular instance), a
file of ignore rules that applies to that one instance and is never committed. It holds the rules that
describe one machine or one person's workflow, such as the build outputs of a platform the repository's
other users do not build, which would be wrong for everyone else in `.loreignore`.

## Motivation

- **Some untracked files are specific to one working tree.** A build on one platform writes outputs under
  paths where the repository tracks files for other platforms. On one game repository, a Mac editor build
  writes its outputs under `Binaries/Mac/` directories beside the tracked binaries of other platforms, and
  every `lore status --scan` lists them as untracked entries.
- **The shared file is the wrong place for them.** On the same repository, rules broad enough to cover
  those outputs, such as `**/Binaries/` patterns, also match directories where the repository tracks files
  for others: 955 tracked files under `Engine/Binaries/DotNET`, 526 under `Engine/Binaries/Win64` and 474
  under `Engine/Binaries/Linux`, counted from the repository's tree at the revision measured. An ignore rule
  hides new files that other users add there (see Proposed Design), so a rule that suits one working tree
  is a risk for every other.
- **Today each user carries them by hand.** The user either lives with the noise in every scan, or edits
  `.loreignore` locally and keeps that edit out of every commit.

## Goals / Non-Goals

### Goals

- An instance can ignore entries without changing what any other instance ignores.
- The rules never reach a commit.

### Non-Goals

- Rules for every repository on a machine, as Git's `core.excludesFile` gives. This proposal is per
  instance.
- Changing what `.loreignore` means, how it is matched, or which operations apply it.

## Proposed Design

- **The file.** The instance's Lore directory holds an optional `ignore` file: `.lore/ignore` for a regular
  instance, and the same name in the external directory of an SWFS instance or the `.urc/` directory of a
  legacy one, wherever `get_dot_lore_path` in `lore-revision/src/repository.rs` puts the instance's other
  files. It holds one rule per line, in the syntax of `.loreignore` (`parse_filter` in
  `lore-revision/src/filter.rs`). Lore skips blank lines and lines starting with `#`, and `\!` starts a rule
  for a name beginning with `!`, as in `.loreignore`. A missing file adds no rules. (Goal 1.)
- **Exclusions only.** Lore applies the rules after those of `.loreignore`, or of a legacy `.urcignore` where
  `.loreignore` is absent, and they can only add exclusions. Lore skips a `!` re-inclusion with a warning
  that names the file and the line, each time it loads the filter, so the file can never bring back
  something the repository ignores. Skipping a line fails toward excluding more, never less, which is the
  direction `parse_filter` protects.
- **Errors as for `.loreignore`.** Lore reads the file the way `filter::load_filter` in
  `lore-revision/src/filter.rs` reads `.loreignore`: a file it cannot read adds no rules, and a file it
  cannot decode fails the instance's filter load with an error naming the file. Every rule Lore keeps from
  the file is an exclusion, and an exclusion always parses.
- **Same scope as `.loreignore`.** The rules join the instance's ignore filter, so they apply wherever
  `.loreignore` does: the working-directory operations (`status`, the dirty scan, `stage`, `unstage`,
  `lock`, and `diff` against the working tree), and not the committed-state ones (`sync`, `switch`,
  `reset`, and `diff` between revisions), as [system design §12.3-12.4](../explanation/system-design.md)
  describes. Like a `.loreignore` rule, a rule here applies only to paths the tree does not track
  ([ADR-00027](../developing/decisions/00027-ignore-filter-applies-to-untracked-paths-only.md)): changes to
  tracked files under it keep flowing through, and the global `--force` flag stages an untracked file anyway.
- **Never committed.** Lore excludes its own directory in the ignore filter it builds (`filter::load` in
  `lore-revision/src/filter.rs`), so the file never reaches a commit. (Goal 2.)

## Compatibility

- **Wire format** — N/A: no message or fragment changes.
- **Client/server protocols** — N/A: the server and the client-to-service IPC are unchanged.
- **On-disk format** — New file `ignore` in the instance's Lore directory, plain text in the syntax of
  `.loreignore`. An older client reads only `.loreignore`, `.urcignore` and the view file
  (`load_filter` in `lore-revision/src/repository.rs`), so it ignores the new file and reports the entries
  it excludes as it does today.
- **CLI and public API** — No new commands or flags. Commands that list or stage working-directory changes
  leave out the paths the file excludes, as for `.loreignore`; a script that counts them can observe that.
  `LoreEvent::FilterExclude` reports those paths with `FilterReason::Ignore`, the reason `.loreignore`
  exclusions carry.

## Non-Functional Considerations

- **Concurrency** — Preserved: Lore reads the file when it loads the instance's filter, as it reads
  `.loreignore`.
- **Memory** — Preserved: its rules join the ignore filter, sized by the number of rules.
- **Statelessness** — Preserved: no state outside the instance's filter.
- **Determinism** — Two instances with different `ignore` files can report and stage different changes for
  the same tree, as an untracked file one excludes stays out of its status and stage. That is the purpose
  of the file, and it is the same property `.loreignore` already has across revisions. History and the committed-state
  operations are unchanged.
- **Observability** — An exclusion event cannot tell this file's rules from `.loreignore`'s: both carry
  `FilterReason::Ignore`.

## Migration Plan

N/A — no breaking changes, no migration required.

## Security Considerations

No change to the trust model. The file is the user's own, in the instance's directory, with the user's
permissions, and it can only exclude. A rule can hide an untracked file from the user's
own status and stage in that one instance, exactly as a local edit to `.loreignore` can today; it changes
nothing another instance or the server sees.

## Privacy Considerations

The file's rules never leave the machine, since Lore never commits its own directory. The paths it
excludes are reported to event consumers in the process, such as bindings and integrations, as
`.loreignore` exclusions are today. No new data reaches the server.

## Risks and Assumptions

**Assumptions**

- **Assumption:** per-instance rules need only exclusions — *invalidated if:* users need to re-include
  something `.loreignore` excludes, for one working tree only.

**Risks**

- **Risk:** a rule hides a new file the user meant to add, which a scanning stage then leaves out of a
  commit — *mitigation:* the scope is the same as `.loreignore`'s, and changes to tracked files are never
  hidden; the user's own rules decide it, and `lore --force stage <path>` stages such a file.
- **Risk:** a file Lore cannot decode stops every command that loads the instance's filter — *mitigation:* the error
  names the file, which is the user's own to fix, as a malformed `.loreignore` does today.
- **Risk:** a user forgets a rule there, and a new file they meant to add is not reported — *mitigation:*
  accepted; `lore --force stage <path>` stages it, as for any ignored entry.

## Drawbacks

- What an instance ignores now depends on two files, one of them not in the repository.
- A user cannot tell from an exclusion event which of the two files hid a path.

## Alternatives Considered

### Status quo: a local edit to `.loreignore`

*Rejected because:* the edit is a change to a tracked file, so it shows in every status and is one scanning
stage away from a commit that imposes the rules on everyone.

### Add the rules to `.loreignore`

*Rejected because:* the rules are right for one working tree and wrong for others. On the repository above
they would hide new files others add in directories where the repository tracks about
2,000 files.

### Exclude the paths through the instance's view

The view (`.lore/view`) is already per instance and never committed.

*Rejected because:* the view decides which tracked paths are materialized and synced, so excluding a
directory there also removes the tracked files under it from the working tree. An ignore rule leaves
tracked content in place.

### Rules for every repository on the machine

A user-level ignore file, read for every instance, as Git's `core.excludesFile`.

*Rejected because:* the rules describe one tree's build outputs, not the user's habits across repositories.
A path rule that suits one repository is wrong in another. It could be added later without changing this
design.

## Prior Art

- Git's `$GIT_DIR/info/exclude` holds ignore rules for one repository that are never committed
  ([gitignore](https://git-scm.com/docs/gitignore)). This proposal follows it, except that the rules here can
  only exclude.
- Mercurial reads extra ignore files named by `ui.ignore` in a repository's own `.hg/hgrc`, which is never
  committed ([hgrc](https://www.mercurial-scm.org/doc/hgrc.5.html)).
- Perforce's `P4IGNORE` names a list of ignore files, so a user can layer a personal file over a shared one
  ([P4IGNORE](https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/P4IGNORE.html)).

## Unresolved Questions

- Should exclusion events name which file a rule came from, with a reason of its own?
