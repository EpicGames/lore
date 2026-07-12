---
lep: 2026-07-11-revision-show-command
title: "`revision show`: a git-show-style command for a single revision, including the root"
authors:
  - Ryan Carbotte
status: Draft
created: 2026-07-11
updated: 2026-07-11
discussion: <TBD — fill in CR link when discussion CR is opened>
---

# `revision show`: A Git-Show-Style Command for a Single Revision, Including the Root

## Summary

Lore has no single command equivalent to `git show <revision>` — commit metadata plus the full unified-diff patch
for every file the revision changed. Reconstructing that view today requires two separate commands
(`lore revision info --delta` for the metadata and file-action summary, `lore file diff` for the content) and, for
a repository's root revision, the second command cannot be made to work at all: the root has no parent, and naming
a nonexistent parent through revision syntax (`<root>~1`) fails today exactly as the equivalent `git diff
<root>~1 <root>` fails in Git. This proposal adds a `lore revision show <revision>` command that renders the
combined view directly, and internally — not through revision-string resolution — special-cases a root revision by
sourcing its diff from an empty predecessor state, the same way Git's own `git show`/`git log -p --root` do
internally for a rootless commit.

## Motivation

`lore revision info <revision> --delta` prints a revision's message, branch, and date, plus a per-file
`A`/`M`/`D` action summary — but no content. `lore file diff --source <revision_source> --target <revision_target>`
prints full unified-diff content — but requires two already-resolved revisions, so showing "what a single revision
changed" means the caller must separately compute that revision's parent and pass it as `--source`. For any
revision after the root, `<revision>~1` does this:

```
$ lore file diff --source @2~1 --target @2
new_script.gd
--- /dev/null
+++ new_script.gd
@@ -0,0 +1,11 @@
+extends Node
...
```

(Reproduced against a real repository; omitting `<paths>` diffs every file the revision touched, giving the
per-file patch body of a `git show`-style view.)

For the root revision this fails, because the root has no parent for `~1` to name:

```
$ lore file diff --source @1~1 --target @1
[Error] revision not found: @1~1
  at lore-revision\src\file\diff.rs:147:13
```

`revision::resolve()`'s ancestor-offset walk (`lore-revision/src/revision.rs:1370-1386`) returns
`RevisionNotFound` the moment it hits a zero parent before the requested offset is exhausted. This is not a bug to
route around: `git diff <root>~1 <root>` fails identically in Git, for the identical reason — the ancestor a bare
`~N` walk asks for genuinely does not exist. Git resolves the underlying user need (see the initial commit's
contents) a different way: `git show <root-commit>` and `git log -p --root` special-case a parentless commit
internally, diffing it against the well-known empty tree object
(`4b825dc642cb6eb9a060e54bf8d69288fbee4904`), without ever asking general revision-offset syntax to produce that
empty tree as something nameable by `~N`. `git diff <root>~1 <root>` still fails in Git today, and correctly so —
"the commit one before the first commit" is not a thing, and revision-offset resolution should say so.

Lore should follow the same split: keep `~N` failing past the root everywhere it's used today (`file diff`,
`branch log`, `revision info`, `revision find`, ...), and add the `git show`-equivalent command as the place that
internally knows how to substitute an empty predecessor for a rootless revision. This also closes the general gap
that there is no single-command "what did this revision do" view in Lore at all — today's two-piece workaround
already has to be composed by hand even for ordinary, non-root revisions.

## Goals / Non-Goals

### Goals

1. Add `lore revision show <revision>`, printing the same metadata `revision info` prints (message, branch, date,
   signature) followed by the full unified-diff patch for every file the revision changed — a single-command
   equivalent of `git show <revision>`.
2. When `<revision>` is a repository's root revision, `revision show` renders every file in it as an addition with
   full content, without requiring the caller to name the root's nonexistent parent through any revision syntax.
3. Leave `revision::resolve()` and its `~N` ancestor-offset semantics completely unchanged. Every existing
   consumer — `file diff`, `branch log`, `revision info`, `revision find`, and any future caller — keeps failing
   with `RevisionNotFound` when asked to walk an offset past the root, matching Git's own behavior for
   `git diff <root>~N <root>`.

### Non-Goals

- Changing `file diff` (or any other existing command) so that `--source <root>~1` succeeds. Per Goal 3, that
  continues to fail, on purpose, matching Git.
- Three-way (`--diff3`) output. Diffing "what a single revision changed" is inherently a two-way concept (before
  vs. after that one revision); diff3 exists to reconcile two divergent branches against a common base, which
  doesn't apply to viewing one revision in isolation.
- Fully resolving merge-revision semantics. `revision info --delta`'s existing delta-block reader already has a
  known limitation here (`lore-revision/src/revision/info.rs:221`, `// TODO: Take merges into account when looking
  up information in the parent.`); `revision show` reuses that same delta block and inherits the same limitation
  rather than solving it as part of this proposal.
- Multi-revision ranges or comparing two named revisions (`git show <a> <b>`, `git diff <a>..<b>`). `revision show`
  takes exactly one revision, matching `git show <revision>`'s single-argument form; `file diff` and
  `revision diff` remain the two-revision tools.
- Path filtering (`revision show <revision> -- <path>`, mirroring `git show <rev> -- <path>`). Worth adding later
  for parity with `revision diff`'s existing `--path`, but out of scope for the initial command.

## Proposed Design

### Reuse `revision info`'s metadata emission

`revision show` resolves `<revision>` exactly as `revision info` does today — via `revision::resolve()` with its
existing, unmodified error behavior, or `load_current_anchor` when no revision is named
(`lore-revision/src/revision/info.rs:167-181`) — and emits the same `LoreRevisionInfoEventData` and commit-message
metadata event that `revision info` already emits. No new resolution logic; a bad or nonexistent `<revision>`
argument fails exactly as it does for `revision info` today.

This addresses the metadata half of **Goal 1**.

### Reuse `revision info --delta`'s change-detection, not a live tree diff

The per-file change list comes from the revision's own precomputed delta block
(`state.delta_block()`), the same source `revision info --delta` already reads
(`lore-revision/src/revision/info.rs:199-303`) — not from a live two-state tree diff
(`diff::diff_revision_paths`, which is what `file diff` uses). This is a deliberate choice over the "synthesize an
empty `State` and run it through the live diff pipeline" approach considered and discarded during this LEP's
drafting (see Alternatives Considered): a revision's delta block already records, per node, whether it was Added,
Modified, Deleted, or Moved — and for a root revision, every entry is necessarily an `Add`, since nothing existed
before it to modify, delete, or move from. Reading that block never requires constructing or diffing against a
predecessor state for a root revision, sidestepping the empty-state question entirely for exactly the case this
proposal targets.

For each delta entry, `revision show` reads content by action:

- **Add** — read the file's content at `<revision>`'s state. No parent state needed, at any revision (root or
  not) — this is the case that fully resolves Goal 2.
- **Modify** or **Delete** — read the file's current-side content (Modify only) from `<revision>`'s state, and its
  previous-side content from the parent state (`state.parent_self()`, deserialized lazily on first need, exactly as
  `revision info --delta` already does for Delete entries at `lore-revision/src/revision/info.rs:210-217`, extended
  here to also cover Modify).
- **Move** — needs the file's previous path to read its old-side content. `NodeDelta`
  (`lore-revision/src/node.rs:1710-1719`) records only the node's current position, not a from-path, so recovering
  it requires looking the node up by ID in the parent state's tree rather than by path. This is flagged as an open
  question below rather than fully specified here.

This addresses **Goal 2**: no code path in this design ever needs an empty `State` object, because a root
revision's delta block contains only `Add` entries, and `Add` entries never read the parent side.

### Reuse `file diff`'s content-rendering, not its change-detection

Once old/new content bytes are in hand for a delta entry, `revision show` builds its unified-diff patch using the
same binary-detection and patch-formatting helpers `file::diff` already implements — `make_diff_content`,
`build_unified_patch`, and the `LoreFileDiffEventData` emission shape (`lore-revision/src/file/diff.rs:819-875,
1104-1110`). These currently live as private items in `lore-revision/src/file/diff.rs` and need to become `pub(crate)`
(or move to a small shared module both `file::diff` and the new `revision::show` depend on) so `revision show` can
reuse them instead of re-implementing patch formatting.

This keeps exactly one implementation of "how Lore formats a unified diff" in the codebase, addressing the content
half of **Goal 1**.

### CLI surface

A new `lore revision show [<revision>]` subcommand, alongside `revision info` and `revision diff` in
`lore-client/src/cli/commands/revision.rs` (`RevisionShowArgs`, added to `RevisionCommands`). `<revision>` is
optional and defaults to the current revision, matching `RevisionInfoArgs`'s existing shape. The same
diff-formatting flags `file diff` exposes (`-U`/`--context`, `--ignore-space-at-eol`, `--ignore-space-change`) are
threaded through for consistency, since both commands render through the same patch-formatting helpers.

## Compatibility

- **Wire format** — N/A. `revision show` reads state that's already local or already fetched into local/remote
  store access; no new messages.
- **Client/server protocols** — N/A. No new RPCs.
- **On-disk format** — N/A. No revision, node, delta-block, or tree format changes; this proposal only adds a new
  reader over data that already exists (the delta block `revision info --delta` already reads).
- **CLI and public API** — Purely additive: one new subcommand (`lore revision show`) and, if surfaced through
  `lore-capi`/language bindings, one new entry point. No existing command's behavior, output, or exit codes change.
  In particular, `file diff --source <root>~1` continues to fail exactly as it does today (Goal 3) — this proposal
  makes no changes to `revision::resolve()`.

## Non-Functional Considerations

- **Concurrency** — No change; `revision show` is read-only, same model as `revision info` and `file diff`.
- **Memory** — No new buffering model. Content is read and diffed per file, the same as `file diff` does today;
  no whole-revision buffering is introduced.
- **Statelessness** — No new persisted state. Nothing about this command is cached or written to disk.
- **Determinism** — The same revision continues to produce the same output on repeated calls.

## Migration Plan

N/A — no breaking changes, no migration required. This is a new, additive command.

## Security Considerations

No security implications. Every byte `revision show` can display was already readable via `lore file write`,
`lore file dump`, or `lore file diff` against the same revision; this proposal only changes how it's presented (one
combined command instead of two). No read-authorization check is bypassed — the same state-read path and its
authorization apply.

## Privacy Considerations

No privacy implications. No new user identifiers, file paths, or metadata are introduced beyond what
`revision info` and `file diff` already surface separately.

## Risks and Assumptions

**Assumptions**

- **Assumption:** A revision's delta block is a complete and accurate record of every file it changed, sufficient
  to reconstruct the full diff without falling back to a live tree comparison. *Invalidated if:* the delta block
  is found to omit changes under some condition (e.g. certain merge shapes) that a live `diff::diff_revision_paths`
  walk would still catch — in which case `revision show` would need to fall back to the live-diff approach this
  proposal deliberately avoids, at least for that case.

**Risks**

- **Risk:** Move entries need the old path to fetch old-side content, and `NodeDelta` doesn't record it (see
  Proposed Design), so the initial implementation may need to defer full Move support (e.g. rendering a Move as an
  add-only patch with a note, similar to how `revision info --delta` today shows a Move's new path without a "from"
  annotation) until the by-ID parent lookup is designed. *Mitigation:* scope this explicitly in the implementing CR
  rather than blocking the whole command on it; Add/Modify/Delete cover the motivating root-revision case
  completely, since a root revision's delta contains no Moves by construction.
- **Risk:** Reusing `file::diff`'s private patch-formatting helpers across modules increases the coupling between
  `file::diff` and the new `revision::show` module. *Mitigation:* extracting them into a small shared module (noted
  in Proposed Design) keeps the dependency explicit and one-directional (both depend on a shared formatter; neither
  depends on the other) rather than reaching into `file::diff`'s internals directly.

## Drawbacks

- Two different mechanisms now produce "a diff for a set of changed files" in the codebase: the live two-state
  tree diff (`file diff`, `revision diff`) and the delta-block reader (`revision show`, `revision info --delta`).
  A future change to one change-detection path (e.g. a new file action type) needs to be checked against both.
- Move support is incomplete at initial launch (see Risks), so `revision show` on a revision containing a rename
  may render less precisely than `file diff` would for the same rename between two fully-resolved revisions.

## Alternatives Considered

### Teach `file diff --source` (or `revision::resolve()` generally) to treat a root's missing parent as empty

Add an opt-in resolution mode so `--source <root>~1` returns an empty synthetic state instead of failing, letting
the existing `file diff` command handle the root case.

*Rejected because:* it makes `<root>~1` succeed in Lore while the equivalent `<root>~1` still fails in Git,
diverging from the parity this proposal's motivation argues for. It also only solves the content half of the
"what did this revision do" question — a caller still needs a second command (`revision info`) for the message and
branch, whereas a dedicated `show` command solves both in one call, matching Git's actual command shape rather than
Git's revision-syntax shape.

### Synthesize an empty `State` and run it through the existing live tree-diff pipeline

Within the `revision show` design itself, an alternative implementation directly deserializes (or constructs) an
empty `State` for a root revision's "before" side and reuses `diff::diff_revision_paths` (`file diff`'s existing
change-detection), the same as the rejected alternative above, just invoked internally by the new command instead
of exposed through `--source` resolution.

*Rejected because:* it depends on `state::diff`'s subtree walk (`lore-revision/src/state.rs:4341-4425`) correctly
handling a missing root node for a `State` that was never deserialized from real data — behavior that is plausible
but unverified, since every other diff today compares two states that both genuinely exist. The delta-block
approach in the Proposed Design needs no such assumption: `revision info --delta` already proves, today, that
reading a root revision's delta block correctly yields an all-`Add` change set, with zero reliance on constructing
or diffing against a synthetic empty state.

### Add a `--patch` flag to `revision info` instead of a new `show` subcommand

Extend `revision info --delta` with a `--patch` flag that upgrades each delta entry from an action letter to a full
unified diff, mirroring how git's `-p`/`--patch` flag augments `git log`/`git show` rather than being a separate
verb.

*Rejected because:* the explicit ask for this proposal is a new command in the shape of `git show`, and `revision`
already groups single-purpose verbs (`info`, `diff`, `history`, `find`) rather than adding flags onto one of
them; a dedicated `show` command is more discoverable and keeps `revision info --delta`'s existing (already
documented and tested) output shape untouched. This remains a reasonable smaller-footprint alternative if review
prefers it.

## Prior Art

- **Lore's own `revision info --delta`** (`lore-revision/src/revision/info.rs:199-303`) already treats a root
  revision's change set as "every node is an Add," correctly, today — by reading the revision's own precomputed
  delta block rather than live-diffing two `State` trees. It only deserializes a parent state at all when a delta
  entry is a deletion, to recover the pre-change path and file/directory flag
  (`lore-revision/src/revision/info.rs:210-217`); a root revision's delta block never contains deletions (nothing
  can be deleted before anything exists), so that branch simply never triggers at the root. This is the direct
  precedent this proposal's design builds on.
- **Git** rejects `git diff <root>~1 <root>` with "unknown revision or path not in the working tree" for exactly
  the same structural reason Lore does — a root commit has no parent to name, and generic revision-offset syntax
  correctly says so. Git's `git show <root-commit>` and `git log -p --root` work around this at the command level,
  not the revision-syntax level: internally, they diff a parentless commit against the well-known empty tree object
  (`4b825dc642cb6eb9a060e54bf8d69288fbee4904`) rather than making `~N` resolve past the root. This proposal follows
  the same split.

## Unresolved Questions

- How should `revision show` recover a Move entry's previous path, given `NodeDelta` records only the node's
  current position? Candidates include looking the node ID up directly in the parent state's tree (if node IDs are
  stable across the rename) or extending the delta block itself to record a from-path for Move entries — the
  latter would be an on-disk format change and needs its own scrutiny if pursued.
- Should `revision show` support `--diff3`-style output for merge revisions specifically (showing the revision's
  contribution relative to each parent), given the existing "TODO: take merges into account" gap in
  `revision info --delta`, or is inheriting that gap acceptable for the initial version?
- Should the shared patch-formatting helpers extracted from `lore-revision/src/file/diff.rs` live in a new module
  (e.g. `lore-revision/src/diff_render.rs`) or under an existing one (e.g. `lore-revision/src/change.rs`)?
