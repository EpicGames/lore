---
lep: 2026-06-21-changesets
title: Changesets
authors:
  - Mattias Jansson
status: Draft
created: 2026-06-21
updated: 2026-06-26
discussion: <TBD — open the discussion PR>
replaces: 2026-05-03-modified-file-tracking
---

# Changesets

## Summary

This proposal lets a single Lore instance carry several ongoing streams of work in one working tree. It introduces the
**changeset**: a lightweight set of changes recorded as its own strictly linear line of history — a line that reduces
to a single net change — layered over the branch the developer is on.

A developer can **attach** a changeset to materialize its changes in the working tree, **detach** it to park them
off-disk, keep several attached at once, let an editor or tool checkpoint work into one continuously, and move a
changeset as a unit — committing it onto the current branch, or switching to another to land it there.

One mechanism thereby covers a wide range of needs that today are unmet or served only by separate, partial
workarounds: automatic personal backup of work, several work streams in flight at once, carrying work across branch
switches, deferring a conflict to resolve in an orderly fashion rather than head-on, toggling changes on and off to
test combinations, keeping long-lived debug or utility changes switchable, and picking up in-progress work on a
developer's other machines.

Concretely, it folds three mechanisms Lore has today — dirty-file tracking, staging as the recording of commit intent,
and the personal backup branch already used in UEFN — into a single concept, and on that foundation adds what Lore has
never had: multiple parallel changesets in one working tree and first-class conflict handling.

## Motivation

A developer working in a Lore repository rarely has just one thing in flight. Over a single sitting, one working tree
accumulates an urgent fix started on top of a half-finished feature, a broad rename touching dozens of files beside a
couple of unrelated one-line corrections, a debug line kept only for local troubleshooting, and a second attempt at a
function whose fate is undecided. These are distinct streams of work. Each is destined to become its own revision, on
its own schedule and often on its own branch — and some are never meant to be committed at all.

Lore gives a repository instance a single current branch and a single working tree, and every local modification in
that tree collapses into one undifferentiated set. The tree records *that* a file changed, not *which* stream the
change belongs to. The moment a developer is doing two things at once, that single set stops matching how they think
about their work, and the cost shows up in several ways:

1. **Concurrent streams can't be kept apart while in progress.** There is nowhere to say "these changes are the fix,
   those are the feature." The grouping lives only in the developer's head and must be reconstructed from memory at
   commit time. When two streams touch the *same* file — a feature edit and a debug print three lines apart — even a
   file-by-file sort can't separate them; the boundary the developer cares about is sometimes a hunk.

2. **Setting a stream aside means losing it or freezing everything.** To get one stream out of the way, a developer
   must either commit it onto the branch before it is ready, or switch branches — which evicts *every* other
   in-progress stream from the working tree at once. There is no way to park one stream while the others stay live and
   editable.

3. **Moving a stream to another branch is manual reconstruction.** Work often starts on the wrong base, turns out to
   belong on a new branch, or needs to land on a specific existing one. Redirecting an in-progress stream onto a
   different branch means re-deriving and re-applying its changes by hand, with no record of which changes made up the
   stream.

4. **In-progress work isn't captured continuously or durably, and is trapped on one machine.** Until a developer
   commits, a stream exists only as uncommitted working-tree state — vulnerable to loss and invisible to tooling. No
   editor or build tool can checkpoint the work of a given stream automatically as it happens, so the natural unit for
   incremental, durable backup simply doesn't exist — and what state there is stays on the one machine it was typed
   on, neither backed up off-machine nor reachable from the developer's other machines.

5. **The only real separation moves the work out of the tree.** Keeping streams genuinely apart today means separate
   branches or separate instances. Both separate work in *time* (you switch between them) or in *physical space on
   disk* (a second checkout to manage) — neither lets several streams stay co-resident, visible, and editable in the
   *same* tree at once, which is exactly where the work is happening. And each workaround is itself overhead to run
   and keep track of: branches to name and switch, instances to clone and manage, stashes to juggle.

6. **There is no way to toggle changes in and out.** Developers routinely want to test a combination of in-progress
   changes, switch one out to isolate a problem, or keep a debug print or local utility tweak around indefinitely
   without it ever shipping. Today a change is either present in the working tree or reverted; there is no durable,
   switchable on/off state for a set of changes.

The result is that developers either serialize naturally-parallel work, pay the overhead of extra branches and
checkouts to run it in parallel, or let everything pile into one tree and disentangle it by hand — re-deriving
boundaries that were obvious while the code was written and are easy to get wrong afterward.

Conflicts add a second kind of friction, orthogonal to keeping streams apart. When in-progress work does collide —
with an incoming sync, a branch switch, or another stream touching the same lines — resolution today is a blocking,
all-or-nothing modal: the developer must resolve or abort *right now*, before anything else can proceed. There is no
way to set a conflict aside and keep working a different stream, to defer a low-priority clash until later, to back up
a half-finished resolution, or to pick one up on another machine — and a single unresolved conflict freezes the whole
tree. The more continuously work is captured and the more often upstream moves under it, the more this all-or-nothing
model bites.

The continuous-capture need is not hypothetical. Unreal Editor for Fortnite (UEFN) already relies on it today: the
editor auto-saves every change a creator makes and commits it onto a per-user backup branch kept hidden from the
creator, purely so that nothing is ever lost. That pattern works and is in production — but it is a *single*, hidden,
unmanaged branch. A creator cannot see it as several streams, organize in-progress work into separate units, park one
while keeping others live, or move a unit onto a different branch. It captures work durably while leaving every other
need above unmet. The Lore CLI and library is unaware of this distinction between a hidden backup stream and a real
branch.

This matters more as Lore moves into large monorepos, where duplicating a checkout per stream is expensive, and as
automated tooling and agents produce changes a human must later sort and attribute. The recent move to make per-file
modification state a first-class, persistently tracked concept
([`2026-05-03-modified-file-tracking`](2026-05-03-modified-file-tracking.md)) closed one part of this gap; the
remaining need is to let a single instance carry **several ongoing streams of work in one tree** — separable down to
the hunk, parkable without disturbing the others, movable between branches, and continuously and durably captured as
work happens.

## Goals / Non-Goals

### Goals

1. **Carry several streams of work co-resident in one instance and one working tree.** A developer should keep
   multiple in-progress streams live at the same time without a second checkout and without switching branches.
   *(Motivation 1, 5.)*

2. **Separate streams down to the hunk, not just the file.** Two changes in the same file should be assignable to
   different streams. *(Motivation 1.)*

3. **Park and restore a stream without disturbing the others.** A developer should remove one stream's changes from
   the working tree and bring them back later, with the other streams untouched and without committing the parked
   stream onto a branch prematurely. *(Motivation 2.)*

4. **Capture a stream's work continuously and durably, on every machine.** An editor or tool should be able to
   checkpoint in-progress work automatically as work happens, producing durable history that survives a lost working
   tree; when remote-synced, that history is also an off-machine backup the developer can pick up on another of their
   machines. *(Motivation 4.)*

5. **Treat a stream as a movable unit.** A developer should move a whole stream onto a chosen existing branch or a new
   one — by switching to it and committing — without re-applying its changes by hand. *(Motivation 3.)*

6. **Make managing many streams cheap.** Listing, attaching, detaching, and reassigning changes between streams should
   be low-overhead operations a developer runs routinely. *(Motivation 5.)*

7. **Toggle changes on and off.** A developer should switch a set of changes in and out of the working tree at will —
   to test combinations of in-progress work, isolate a problem, or keep long-lived debug and utility changes that
   never ship. *(Motivation 6.)*

8. **Defer and resolve conflicts in an orderly fashion.** When work conflicts, a developer should be able to set the
   conflict aside as captured, recoverable state and resolve it later — incrementally, without it freezing other work
   or being lost — instead of resolving or aborting on the spot. *(Motivation: conflict friction.)*

### Non-Goals

- **Replacing regular branches or the remote review flow.** Changesets are a working-state mechanism that feeds the
  existing branch/commit/push/change-request workflow, not a substitute for it.

- **Branching off a changeset.** A changeset is a single linear stream that reduces to one net change; nothing is ever
  based on it, and it never forks into sub-branches.

- **Merging to or from a changeset.** A changeset reaches a regular branch only by `commit` (squashing to its single
  net change); it is never a merge source or target.

- **Co-editing a changeset.** A changeset has a single writer, its creator (who works it from any of their own
  machines). A different user cannot write to it — at most they copy it onto their own ground as a new, locally-owned
  changeset, a side benefit rather than collaboration. Genuine collaboration comes from committing the changeset as a
  new full branch (or onto an existing branch) that others develop on, never from co-writing the changeset itself.

- **Automatic semantic merging of overlapping streams.** Attached streams are required to be cleanly mergeable, so
  their composition is a deterministic, order-independent overlay; the proposal does not auto-merge *conflicting*
  overlaps — a clash between two attached streams is resolved by reassigning or detaching a change, never by an
  automatic semantic merge.

- **A new content or diff format.** Changesets reuse Lore revisions and the existing merkle tree; they do not
  introduce a separate patch representation.

## Proposed Design

This proposal introduces the **changeset**: a lightweight set of changes, held as its own line of history and layered
over the branch the developer is on. Where a regular branch *represents a line of history*, a changeset *represents a
single set of changes to apply* — recorded internally as a branch (a line-of-checkpoints branch reusing Lore's branch
machinery), but understood and manipulated as one delta.

**Core invariant.** This is essentially set theory. Within the current view, the working set on the filesystem is
*exactly* the current revision plus the sum of the attached changesets — `working set = current revision + Σ(attached
changesets)`, with no slack on either side. Attach adds a changeset to that sum and detach removes it; every change
that appears in the working set is recorded into exactly one attached changeset — the default unless another already
owns the affected region — so the invariant holds continuously. Two refinements keep the equality exact rather than
break it: an *intent-only* file counts as if its working-set content were already captured — the content is logically
part of the changeset, only its physical recording is deferred to commit (see *Intent-only capture*); and in a sparse
working tree both sides are read view-filtered — `current revision` means the view-filtered current revision and each
changeset contributes only its in-view content, so out-of-view content sits outside the equation until the view
expands (see *View filter*). Because attached changesets must be cleanly mergeable (no two touch the same region of a
file), the sum is well-defined and independent of attach order. Every operation below is just a way of maintaining
this invariant.

**Ground and base.** The **ground** is the instance's current regular branch and its current revision — the real
branch you are checked out on, "where you are" in the revision graph; it is per-instance and may differ from one of
the creator's machines to the next. A changeset is a line of **content** checkpoints rooted at the revision it was
created from (its *base*). The base is only that origin: a changeset's `latest` is always kept current with the ground
— attach and ground advancement merge the ground in (below) — so the changeset's net change is simply the **two-way
diff between the current ground and its `latest`**, and the base is not used to compute or commit the delta. (When the
changeset is conflicted with the ground, its `latest` holds diff3 markers; its represented change is then the file's
last *clean* checkpoint and the marker `latest` is transient materialization state — see *Conflicts are first-class*.)
What syncs across the creator's machines is the content checkpoint line itself; each machine derives the delta locally
against its own ground. (*Goal 1.*)

Visually, it is an ordinary multi-branch revision graph. You are grounded on one current branch; the changesets layer
on top of that ground (the figure shows the v2 picture, with two changesets attached at once):

```text
   main   ●────●────●────●   r4   ◄── ground: current branch (main), current revision (r4)
          r1   r2   r3   r4
                │    │
                │    └─▶──●──●──●  feature/login (another branch — forked at r3, not current)
                └─▶──●──●          release/1.0   (another branch — forked at r2, not current)

   the ATTACHED changesets are applied on top of the ground (main @ r4) in the working tree:

          r4 ──┬──▶  ui-tweak     ●──●──●   (≡ one net change)
               └──▶  debug-logs   ●         (≡ one net change)

   DETACHED — recorded but parked off-disk (not in the working tree); each one is
   materialized onto the ground at the moment it is attached:

          bugfix        ●──●          (≡ one net change)
          experiment    ●──●──●──●    (≡ one net change)

   legend:  ●  a revision        ●──●──●  a changeset's line of checkpoints
            ≡  the whole changeset squashed to one net change; `commit` writes it
               as one new revision on the current branch
```

**The default changeset.** Every instance always has an unnamed, always-attached default changeset —
**remote-synced**, so unattributed work is backed up off-machine by default (see *Local-only or remote-synced*). Any
change to a file not assigned to a specific changeset is captured there, so by default *all* local work is already
attributed to a changeset; recording with `lore dirty --local` keeps that checkpoint on the machine (committed, not
pushed). The developer opts into organization by moving files (and, from v2, hunks) out of the default into separate
changesets — not by opting in to tracking at all. The default cannot be discarded; it is cleared only by reverting its
changes, and persists as the empty catch-all. (*Goals 1, 2, 6.*)

**No names.** A changeset has no name; it is identified by an internal id — there is no name→id key as regular
branches have — and an optional `description` in its metadata carries a human-readable note of what it contains and
represents. The default changeset is the catch-all for unattributed changes and carries no description; a developer
describes a changeset rather than naming it. (*Goal 6.*)

**Presentation.** Changesets are presented separately from regular branches: they do not appear in `lore branch list`.
A dedicated `lore changeset` listing shows them, each marked *attached* or *detached*. Keeping the two kinds in
distinct views avoids blurring the regular-branch concept — a regular branch is mutually exclusive in the working
tree, a changeset additive. (*Goal 6.*)

**This folds dirty tracking and the staged area into the changeset.** By default, local work is recorded as checkpoint
commits on a changeset — by an editor or filesystem watcher, or by a CLI/API call (`lore dirty`, the same call editors
and watchers use) that tells Lore of a change. A file's modification state becomes simply its membership in a
changeset, replacing the per-file *dirty* flag from
[`2026-05-03-modified-file-tracking`](2026-05-03-modified-file-tracking.md). The separate staged *area* — today a
second tree holding snapshotted content — goes away, since the changeset already holds the content; `lore stage`
survives only as a thin selector marking which members to include in the next commit, with no separate staged storage.
Membership is file-granular in v1 and hunk-granular from v2. `lore status` reports each attached changeset and the
changes it holds, and `lore commit` commits a changeset onto the current branch — the whole changeset by default, or
just the staged subset when one is marked (see *Unit operation: commit*). This proposal supersedes that LEP, keeping
its `--scan` reconciliation (see *Scanning*) while replacing the per-file dirty flag. (*Motivation 4.*)

**The single-changeset case is today's behavior, give or take one default.** With only the default changeset — the
configuration a developer who never splits work into streams stays in — the model is the familiar dirty/stage/commit
loop. The default changeset's membership is the dirty set; `lore stage` marks which of those files to commit, exactly
as today; `lore status` shows what it shows today; and committing a staged subset commits that subset and leaves the
rest in the changeset — identical to today's staged commit. The one deliberate difference: committing with **nothing**
staged commits the *whole* changeset rather than being a no-op, so `lore commit` ships all in-progress work.
Changesets are a strict superset: the single-stream workflow is unchanged until the developer creates a second
changeset — yet the unification is already in place underneath. One thing *is* genuinely new even here: the default
changeset is remote-synced, so all in-progress work is continuously backed up to the server by default (`lore dirty
--local` opts out per change) — invisible to the dirty/stage/commit loop, but a real change in posture (see
*Privacy*).

**Scanning.** Scanning becomes reconciliation of the filesystem against the attached changesets: a change to a file
not in any attached changeset is applied to the default changeset, while a change to a file already in an attached
changeset is applied to it. Where a file belongs to more than one attached changeset, attribution follows the same
rule as interactive edits (open question 1).

**A line of history, handled as a unit.** A changeset is a real sequence of revisions, not a single stored diff.
Incremental work accumulates as successive revisions on it, so the changeset carries its own durable, inspectable
history. The developer never manages those revisions individually: `attach`, `detach`, and `commit` all operate on the
changeset as a whole — on its net change against the current ground. This is StGit's "a stack of commits handled as
one patch series" idea applied to a Lore branch. (*Goals 4, 5.*)

**Linear and non-branchable.** A changeset is strictly linear: it has no internal branching, and nothing — neither a
regular branch nor another changeset — can be based on it. Its checkpoint revisions exist only to record the
changeset's progress; what a changeset *means* is its single net change against the current ground, and that is what
`commit` ultimately delivers. Every changeset layers directly on the ground rather than stacking on another changeset,
so they are peers, not a stack; attaching reconciles each against those already attached (see *attach* below). (*Goals
2, 5.*)

**attach / detach.** `attach` materializes a changeset's net change into the working tree; `detach` removes it,
leaving each file as the ground and the remaining attached changesets define it. Several changesets may be attached at
once (in v2), their changes overlaying in the one tree. Detaching parks a changeset off-disk without committing it
onto the ground or evicting the others; it first flushes a checkpoint of the changeset's current content (so no
uncaptured edit is lost), then removes the changeset from the attached set and its contribution from the tree
atomically, so the core invariant holds the instant it completes. Because a conflict is just captured state, a
changeset can always be detached — even mid-conflict, parking with its conflict intact for a later attach. (*Goals 1,
3.*)

**Attach materializes the changeset onto the ground.** Attaching brings the changeset current with the current ground
— a 3-way merge of the changeset's `latest` content with the ground — and lays the result in the working tree, on top
of any already-attached changesets. When the ground has moved under the changeset, that merge — clean, or conflicted
per *Conflicts are first-class* — is **committed as a checkpoint on the changeset** (an ordinary fast-forward append),
so `latest` holds the file's full current content and equals the working copy. Composition with the *other* attached
changesets is different: each attached changeset keeps only its own changes over the ground (it never absorbs another
changeset's), and the working tree simply overlays them. Two attached changesets that touch the same file therefore
**must be cleanly mergeable** — a hard requirement that keeps the composition order-independent (see *Determinism*).
If a changeset's changes conflict with the ground, the attach still **completes**: the conflicted result (diff3
markers) is laid in the tree and recorded as a checkpoint flagged *conflicted*, to be resolved later (see *Conflicts
are first-class*). A conflict with an *already-attached* changeset is different — the clean-mergeability requirement
forbids it, so the developer reassigns or detaches the clashing change rather than overlaying a conflict. Attach only
ever appends to the line — it never rewrites earlier checkpoints, and never folds one attached changeset's changes
into another. Because attached changesets never conflict with *each other* (a ground conflict is the changeset's own
marked region), the attached set always composes consistently in the working tree. (*Goals 1, 3, 5.*)

**Ground advancement.** When the ground advances — a `sync`/`switch`, or a commit onto the current branch — Lore
brings each attached changeset current with the new ground, reusing the attach merge; the merged result is committed
as a fast-forward checkpoint, so the changeset's `latest` tracks the new ground and its net change stays the two-way
diff against it. A changeset that was *already* conflicted does not re-merge its markers — Lore aborts the in-flight
conflict and re-merges the file's last *clean* content against the new ground (yielding a fresh conflict, or none), so
conflicts never compound across grounds (see *Conflicts are first-class*). Detached changesets are left untouched and
are brought current lazily the next time they are attached. (*Goals 1, 3.*)

**Conflicts are first-class.** A conflict between a changeset and the ground is ordinary state, not a modal to clear
before anything else can happen. The conflicted content (diff3 markers) is captured as a checkpoint on the changeset,
flagged *conflicted*, reusing Lore's existing conflict flags — no new format. So a conflicted changeset can be
checkpointed, detached and parked, and synced across the creator's machines, then resolved later — incrementally, file
by file, each resolution step its own checkpoint with full history. Because the conflict is *derived*, not the
changeset's own content, re-grounding stays clean: Lore takes the file's last non-conflicted content — the changeset's
own version, the *theirs* side — and re-merges that against the new ground, aborting the prior conflict rather than
re-merging its markers, so nothing compounds across grounds or machines. The one caveat: in-flight resolution work is
not carried across a re-ground — it stays recorded on the changeset (recoverable), and a later, smarter selection rule
could re-ground from it instead of the last clean content, with no format change. The one thing a conflict blocks is
committing the conflicted files onto a real branch (see *Unit operation: commit*), so the real branch never receives
markers. Inter-changeset conflicts remain the exception: the clean-mergeability requirement keeps two attached
changesets from conflicting with each other, so the overlay stays well-defined and order-independent — only the
changeset-vs-ground conflict is carried as first-class state. (*Goals 3, 4, 8.*)

**View filter.** In a sparse working tree, attach materializes only the changeset's in-view paths; changes the
changeset carries outside the current view stay recorded and commit in full, faulting onto disk only if the view later
expands to include them. Changesets thus respect the sparse model rather than silently widening it. (*Goal 1.*)

**No active changeset.** There is no *active* changeset and no per-instance pointer that selects where new work lands.
A change to a file already held by an attached changeset flows into that changeset; a change to a file in no attached
changeset flows into the default changeset, which is kept attached for exactly this purpose. Incremental checkpoints
follow the same rule. To build up a specific non-default changeset, a developer edits and then reassigns the change
out of the default changeset into that changeset — cheap enough (see *Reassigning changes*) that routing every edit
through a stateful *active* selector is not worth its complexity. (*Goals 1, 6.*)

**Hunk-level membership (v2).** A change's membership in a changeset is recorded at hunk granularity, so two edits in
the same file can belong to different changesets. This needs no patch format: each changeset stores the file's full
content as that changeset sees it — the ground's file with only that changeset's hunks applied — and composition is an
ordinary 3-way merge against the ground base, with hunk membership a presentation layer over those whole-file
snapshots. This is a v2 capability; v1 tracks membership per whole file. How membership is presented and reassigned
across attached changesets is refined in open question 1. (*Goal 2.*)

**Reassigning changes.** A developer or tool can move or copy a file's changes — or, from v2, an individual hunk's —
from one changeset to another. This is how work is organized into changesets after the fact: pull a stray edit out of
the default changeset into the changeset it belongs to, or copy a shared fix into a second changeset that also needs
it. (*Goals 2, 6.*)

**Reverting vs. unstaging.** These are different operations. `lore unstage` deselects a file from the next commit's
staged subset — the change stays in the changeset, just excluded from this commit (the inverse of `lore stage`). To
*drop* a change outright, a developer reverts the affected file in the changeset; because the changeset keeps its line
of history, the change can be resurrected later by reverting to an earlier revision on it. Dropping a change is just
another step on the changeset, not a separate staging state. (*Goals 4, 6.*)

**Incremental capture.** An editor, watcher, or build tool can request a checkpoint; Lore records a lightweight
revision on each attached changeset that holds uncaptured changes — the default changeset catches anything attributed
nowhere else. This gives continuous, durable backup per changeset as work happens, with no manual commit step. Capture
runs even mid-conflict — a conflicted state is checkpointed and flagged like any other (see *Conflicts are
first-class*), so an in-progress resolution is itself backed up. Their cadence and coalescing are an open question
(below). (*Goal 4.*)

**Intent-only capture and large files.** Capture has two modes per file, chosen when the change is *recorded* into a
changeset (`lore dirty`), not when it is staged. By default a file's changes are recorded in full as checkpoint
revisions, giving the continuous durable backup above. A file can instead be recorded *intent-only*: the changeset
notes that the file belongs to it and will be committed, but its content is captured only at commit time, not on every
checkpoint. To avoid writing large amounts of data while iterating on a large file, the automatic policy records files
above a configurable per-repository size threshold (8 MiB by default) as intent-only. The trade-off is that an
intent-only file has no continuous backup trail between commits. (*Goals 4, 6.*)

**Unit operation: commit.** Re-grounding a changeset onto a different revision or branch needs no dedicated operation:
syncing or switching the ground re-grounds every attached changeset automatically (the merge appends a fast-forward
checkpoint, *Ground advancement* above), so the only unit operation is `commit` — and it always lands on the **current
branch at its tip**. `commit` squashes the changeset's line of history into its single net change and writes that as a
single new revision on the current branch; because the changeset is already materialized on the ground, this is a
clean append, never a cross-branch merge. The current revision must be the branch's latest — if the tip has advanced
past it, `commit` is refused until the developer `sync`s up to latest (re-grounding the attached changesets) or resets
the branch's latest pointer back to the current revision (the existing regular-branch tip reset). `commit` is likewise
refused while any file in the committed set is conflicted, so the real branch never receives diff3 markers — the
developer resolves first, or commits only the clean files via the staged subset. By default it lands the changeset's
*whole* net change; if a subset is staged (`lore stage`), it lands only the staged files; the unstaged changes carry
onto a changeset rooted at the new revision — the default persists as itself (re-grounded), while a non-default
changeset's remainder moves to a fresh changeset and the emptied original is discarded. This is the classic
stage-a-subset commit, expressed as commit-then-forward. To land a changeset on a *different* branch, the developer
`switch`es to it first — switching re-grounds every attached changeset onto that branch; any conflict against it
becomes the changeset's own marked state, resolved before committing there. There is no commit onto an arbitrary
target. After a whole-changeset commit the changeset's net change is now in the ground, so it is empty; Lore
auto-discards it, keeping its checkpoint line briefly for local recovery before garbage-collecting it. The default
changeset is never discarded — it persists, re-grounded onto the new revision, as the empty catch-all. A changeset is
never a merge source or target: it reaches a regular branch only by `commit`, never by merge. Together these let a
developer move a changeset between branches and decide where and when it ships, treating the whole changeset as one
unit. (*Goals 3, 5.*)

**Local-only or remote-synced.** Every changeset's checkpoints land on its backing branch the same way; the only
difference is whether the commit is **pushed**. A remote-synced changeset pushes each checkpoint revision to the
server, so the creator's in-progress work is backed up off-machine and available on the creator's other machines — a
changeset started on a desktop can be picked up on a laptop. A local-only changeset commits its checkpoints locally
and never pushes, so the work stays on the instance. **The default is remote-synced**: by default each checkpoint
commit uploads and its revision is pushed, so all local work is durably backed up off-machine with no opt-in — the
UEFN backup model, generalized. The disposition is the local/remote argument to `lore dirty` (default *remote*); `lore
dirty --local` commits the checkpoint locally, with no push. (*Goals 1, 4.*)

**Single writer.** A changeset is owned by its creator, and only the creator updates it. The single writer is the
user, not a machine — the creator works the same remote-synced changeset from any of their own machines, which is how
remote sync doubles as per-user backup and cross-machine pickup. Single ownership removes *foreign* concurrency: no
other user ever advances the changeset, so a rewrite — moving or splitting changes — never has to merge another
person's edits, and there is never a multi-party reconciliation. A multi-writer changeset would instead force every
rewrite to account for other users' changes — remote syncing, merging, and conflict handling on each such rewrite —
which is exactly the cost this constraint avoids.

What single ownership does *not* remove is the creator's own two machines diverging, since they are two writers of one
changeset; cross-machine use is therefore sequential, not simultaneous, kept consistent by primitives the model
already has. Every update the server sees is a **fast-forward append** to the changeset's content line — ordinary edit
checkpoints and the merge checkpoints attach and ground advancement produce alike — guarded by a compare-and-swap on
the changeset's latest-pointer key (the compare-and-swap the mutable store already exposes for branch pointers).
Continuous capture pushes checkpoints as work happens, so the remote tip stays nearly live and picking the changeset
up on another machine is normally a plain fast-forward to the last pushed checkpoint — no reconciliation. The two
machines truly diverge only when both append to the line while one is partitioned from the server with checkpoints
still unpushed; on that machine's next push the compare-and-swap fails, and it replays its local checkpoints onto the
new remote tip, re-linearizing locally so that what the server records is still a fast-forward. Because that conflict
is the creator's own changes against their own, the resolution is the creator's own choice, file by file, not a
multi-party merge. The guarantee is therefore precise: the changeset never reconciles another *user's* edits
unconditionally, and reconciles the creator's own machines only in this rare partitioned-divergence case, which the
local replay settles before a fast-forward push.

As a side benefit, a different user can *copy* a remote-synced changeset — handy for ad-hoc help like "copy my
debug-logging changeset to reproduce the bug": Lore computes the source changeset's net change, creates a new
changeset from the consuming user's current revision, and applies that net change as its first checkpoint, resolving
any conflicts — the same merge-onto-ground mechanism `attach` uses. Reading another user's remote-synced changeset in
order to copy it is governed by the repository's existing read permissions — anyone who can read the repo can copy a
remote-synced changeset, and the creator's choice to remote-sync *is* the opt-in to that exposure (a private changeset
stays `--local` or local-only). The original stays the creator's; the consumer gets an independent, locally-owned copy
on their own ground. This is not a collaboration channel — real collaboration still goes through committing a
changeset as a normal branch that others develop on. (*Goals 3, 5.*)

**Locks.** Changesets interact with the exclusive locks from
[`2026-06-19-successor-locks-unmergeable-files`](2026-06-19-successor-locks-unmergeable-files.md). A lock on an
unmergeable file can be taken at either of two moments: when the file is first edited and tracked by a changeset, or
only when that changeset is committed onto a real branch. Taking it late lets a developer experiment locally without
holding a lock, accepting that the work may fail the lock's causal-safety check when it is committed; taking it early
reserves the file up front when the change is known to be real. The default policy is an open question. (*Goals 4,
7.*)

**Links and layers.** No special treatment is needed. Because a changeset is backed by a branch, its checkpoints,
attach/detach materialization, and `commit` all run through the same branch and commit machinery that already resolves
linked repositories and layers — so a changeset whose changes span a linked repo or a layer is handled intrinsically
by the existing operations, with no changeset-specific link or layer logic. Continuous capture across a link is no
different: a checkpoint of a changeset that spans a linked repo checkpoints the linked state through the same
link-commit path, so its cadence and cost ride the existing mechanism (and the checkpoint-volume Risk applies there
too).

### Phasing

**v1 — the default changeset only.** The first cut ships a single changeset: the unnamed default, always attached.
There are no other changesets yet — no creating, attaching, detaching, or parking additional changesets — so the
working tree is always the ground plus the default. Today's separate staged *area* disappears — the changeset holds
the content — but `lore stage` keeps its classic role of selecting which files to commit (now a flag over changeset
membership), and `lore commit` ships the whole default changeset unless a subset is staged. This already delivers the
dirty-tracking-and-staging unification, the default changeset with continuous capture and the intent-only/large-file
policy, reverting within the changeset, committing the default onto the current branch, and local-or-synced
single-writer backup (off-machine durability and cross-machine pickup) — Goals 4, 5, and 8 and the full unification,
generalizing the UEFN per-user backup branch into a first-class concept. Because there is only the default over the
ground, edit attribution is trivial and the composition and cross-branch-attribution questions (open question 1) do
not arise. Membership is whole-file; hunk-level membership (Goal 2) is deferred to v2.

**v2 — multiple changesets, several attached and overlaid.** The second phase adds non-default changesets and the
operations over them — creating, attaching, detaching, parking, and reassigning changes between changesets — and
overlays several in one tree at once, so the working tree becomes the ground plus the sum of all attached changesets.
This completes Goals 1 (co-resident streams), 3 (park and restore a stream without disturbing the others), and 6
(manage many streams cheaply), adds hunk-level membership (Goal 2), and enables Goal 7 (toggling combinations on and
off). Composition rests on the clean-mergeability requirement (above); the one residual choice is open question 1, and
v1 is unaffected by it.

### Open design questions

The core invariant and the clean-mergeability requirement settle most of v2's attribution. Because two attached
changesets never touch the same region of a file, a file held by several of them is partitioned into disjoint
per-changeset regions over the ground, so a working-tree change attributes to the changeset that owns the region it
touches (and to the default for a ground region) — which is exactly what keeps `working = ground + Σ(attached)`
satisfied. A scan attributes each changed region the same way, and detach/re-attach is clean and order-independent
because the regions do not overlap.

1. **Edit attribution for a region-spanning edit (residual).** The one case the rule above does not decide is a single
   edit that *spans* regions owned by different changesets (or a changeset region and a ground region): whether to
   split it at the region boundary or assign the whole edit to one changeset. Either way the sum must still reproduce
   the working tree, so it is a policy choice with sensible defaults, not a feasibility blocker (it also appears under
   Unresolved Questions).

## Compatibility

- **Wire format** — Additive and backwards-compatible. Local-only changesets never touch the wire. A remote-synced
  changeset's latest pointer and revisions ride the existing branch path — its latest pointer reuses the normal branch
  key type for now — while its changeset metadata (marker, creator, local-only/synced flag, optional description)
  travels under a new dedicated key type keyed by id, with no name→id key. Branch enumeration keys off the name→id and
  id→metadata mappings, never the latest-pointer key, so a v(N-1) client — which has neither a name→id entry nor a
  recognized metadata entry for a changeset — never discovers one; reusing the latest-pointer key type is invisible.
- **Client/server protocols** — Local-only changesets involve no protocol. Remote-synced changesets push and fetch
  like branches, with three constraints the server enforces: a changeset is writable only by its creator; its latest
  pointer advances only by fast-forward compare-and-swap, so a machine that diverged while partitioned replays its
  local checkpoints onto the remote tip and then fast-forwards rather than merging on the server; and another user
  consuming it receives the net change to seed a new, locally-owned changeset rather than gaining write access. What
  syncs is the changeset's content checkpoint line, including the merge checkpoints attach and ground advancement
  append; the attach/detach status and the per-ground materialization are derived locally and never touch the wire.
- **On-disk format** — Additive. A changeset's latest pointer reuses the existing branch latest-pointer key type for
  now, but its metadata lives under a new dedicated mutable key type keyed by id (the changeset marker is implicit in
  the key type; creator, local-only/synced flag, optional description), with no name→id key. Branch enumeration keys
  off the name→id and id→metadata mappings, never the latest-pointer key, so changesets never appear in branch
  listings, any number of them leaves regular-branch metadata untouched, and a branch's type is known from its key
  type without loading metadata. Per-instance state additionally records attach/detach status and each file's
  membership (content-bearing or intent-only, no stored content until commit) — local to the instance, not part of the
  changeset's synced content line. The per-file *dirty* flag from
  [`2026-05-03-modified-file-tracking`](2026-05-03-modified-file-tracking.md) is replaced by changeset membership, so
  existing dirty state migrates into the default changeset on upgrade. An upgraded Lore reads existing repositories; a
  downgraded Lore never discovers changesets (no name→id entry, metadata under a key type it does not read), and the
  Migration Plan covers reconciling its work on the next upgrade.
- **CLI and public API** — New surface for creating, listing, attaching, detaching, moving or copying changes between
  changesets, committing, and discarding. Changesets surface through a dedicated `lore changeset …` command group,
  listed separately from `lore branch` and marked attached or detached. `lore status` now reports every attached
  changeset and the changes it holds rather than reading dirty flags; the default changeset's changes are equivalent
  to today's `lore status` output. `lore dirty` records a change into a changeset (the default, or a specified one)
  with a capture mode — full content, or *intent-only* (content captured only at commit; automatic above a
  per-repository size threshold, a config value defaulting to 8 MiB) — and a local/remote disposition (default
  *remote*; `--local` commits the checkpoint locally, with no push); `--scan` reconciles the filesystem against the
  attached changesets, routing each detected change to the changeset that already holds the file, or, for a file in
  none, to the default changeset. `lore stage` marks which members of a changeset to include in the next commit —
  staging a modified-but-untracked file first records it into the default changeset, then marks it — and `lore
  unstage` is its inverse, deselecting a file (distinct from reverting, which drops the change). `lore commit` commits
  the default changeset onto the current branch: the whole changeset when nothing is staged, or the staged subset
  (forwarding the rest into a new changeset) when a subset is marked. `lore resolve <file>` settles a conflicted file,
  clearing its *conflicted* flag and recording the resolved content as a clean checkpoint, after which `commit`
  accepts it. Creating or configuring a changeset chooses local-only or remote-synced; consuming another user's
  remote-synced changeset creates a new local changeset from the current revision. `lore changeset discard <id>`
  removes a changeset — detaching first if attached, dropping its metadata and checkpoint line (recoverable from local
  history until garbage-collected), and, for a remote-synced changeset, requesting its server-side removal; the
  default changeset cannot be discarded, only cleared by reverting its changes.
- **Configuration** — adds one repository config value, the intent-only size threshold, defaulting to 8 MiB; an
  existing repository without it uses the default.

## Non-Functional Considerations

- **Concurrency** — Changeset write operations — checkpoint commits onto attached changesets, attach/detach, and
  commit — acquire the repository write token, gaining exclusive access and serializing against every other write
  operation, including those of other callers; reads such as `lore status` need no token. Several changesets may be
  attached while a tool issues automatic checkpoints, but those checkpoints take the token in turn. Attach and ground
  advancement always complete under the token — a ground conflict is recorded as the changeset's marked state
  (*Conflicts are first-class*) rather than holding a resolution lock — so there is no mid-resolution window to
  serialize. Across the creator's own machines, a remote-synced changeset's latest pointer advances only by
  fast-forward compare-and-swap; a push that loses the compare-and-swap replays its local checkpoints onto the new tip
  and then fast-forwards, rather than merging on the server, keeping the changeset linear (see *Single writer*).
- **Memory** — Materializing and overlaying net deltas must reuse Lore's streaming, sparse merkle model, streaming
  rather than buffering whole-tree state when attaching or detaching a changeset.
- **Statelessness** — Introduces per-instance persistent state (the changeset set and attach state), held by the same
  machinery that backs today's anchor. No process-global state.
- **Determinism** — The net delta of a changeset — the two-way diff between the current ground and its `latest` — and
  the result of `commit`, must be deterministic for the same inputs. Composition of several attached changesets is
  order-independent **by requirement**: two attached changesets that touch the same file must be cleanly mergeable
  (non-conflicting), so overlaying them in any order yields the same working tree.

## Migration Plan

Two transitions apply even to the instance-local first cut. First, on upgrade existing per-file dirty state becomes
membership in the default changeset, and any staged files become that changeset's staged subset — so both uncommitted
work and the commit selection are preserved; what goes away is the separate staged *area* (its second content tree),
not staging itself. Second, `lore status`, `lore dirty`, `lore stage`, `lore unstage`, and `lore commit` adjust as
described in Compatibility — `lore stage`/`lore unstage` select and deselect over changeset membership, `lore dirty`
carries the capture mode, and `lore commit` ships the whole default changeset when nothing is staged. A downgraded
Lore degrades gracefully: it never discovers changesets — branch listing keys off the name→id and id→metadata
mappings, and changesets sit outside both — so it operates only on regular branches and cannot touch them. The
staged-anchor structure (dirty and staged files) is retained on disk — only the new Lore stops using it — so a
downgraded Lore keeps tracking changes in it exactly as before. On the next upgrade, any nodes the older client left
in the anchor are folded into the default changeset. Remote-synced changesets add the new metadata key type to the
wire (additive); their latest pointer and revisions travel the normal branch path, but an older client never discovers
them and the server refuses pushes from non-creators, so they are safe. This transition is part of the proposal, not
deferred.

## Security Considerations

Local-only changesets do not change the trust model — they never leave the instance, so no malicious peer or crafted
repository can observe or influence them beyond what is already possible for local working state. Remote-synced
changesets send in-progress work and tracking to the server — by default, since the default changeset is
remote-synced, with `lore dirty --local` and local-only changesets as the per-change opt-out. The server enforces
single-writer ownership, so a malicious peer cannot alter another user's changeset; the most it can do with its own is
offer a net change that a consumer explicitly copies and resolves — and read access to a remote-synced changeset
follows the repo's read permissions, no broader — no different in trust terms from consuming any branch. The creator
identity and sync flag on a changeset become integrity-sensitive metadata.

## Privacy Considerations

Local-only changesets expose no new data to other parties — checkpoints stay on the developer's machine. A
remote-synced changeset exposes in-progress content, file paths, and the timing of automatic checkpoints to the
server, where they serve as the creator's backup and cross-machine store. Because the default changeset is
remote-synced, this exposure is the **default**: unless work is recorded with `lore dirty --local` (or moved into a
local-only changeset), a developer's entire working set streams to the server continuously — a deliberate trade of
privacy for durability, and more than deliberate commits reveal. In the side-benefit case where a teammate copies a
changeset, that content reaches the teammate too. `lore changeset discard` plus garbage collection removes a changeset
locally and requests its server-side removal when remote-synced; whether the server can fully expunge it or only
tombstone it is a deletion concern to specify.

## Risks and Assumptions

**Assumptions**

- **Assumption:** developers want streams *co-resident* in one tree, not merely cheaper branch switching —
  *invalidated if:* users only ever work one stream at a time, in which case existing worktree-style instances already
  suffice.
- **Assumption:** carrying a changeset-vs-ground conflict as deferred, first-class state (rather than resolving it at
  attach time) is an acceptable model — *invalidated if:* deferred conflicts pile up unresolved and confuse the
  working state instead of being dealt with in an orderly fashion.

**Risks**

- **Risk:** a changeset is branch-backed yet behaves additively, unlike a mutually-exclusive regular branch, so the
  two could blur — *mitigation:* the external concept is named distinctly (*changeset*, never "branch"), stored under
  a distinct mutable key type, and surfaced through a separate listing/command, so they stay separate in the UI.
- **Risk:** automatic checkpointing floods the local store with short-lived revisions — and in v2 a single save can
  checkpoint *every* attached changeset that changed, multiplying the volume by the number of attached changesets —
  *mitigation:* coalesce or garbage-collect checkpoint revisions, capture large files as intent-only rather than
  checkpointing their content, and squash on commit; coalescing must bound the per-save-times-changesets volume, not
  just one changeset's.

## Drawbacks

- Multiple simultaneously-materialized changesets are a working-tree model no other Lore command assumes, so the
  change reaches staging, status, sync, and branch operations.
- A changeset is branch-backed, so every branch operation must define its behavior for the changeset key type.
- Folding the staged area into the changeset model reworks the commit path — `lore commit` now ships the whole
  changeset when nothing is staged (rather than erroring), and `stage`/`unstage` become selectors over changeset
  membership — which can surprise scripts or integrations that relied on the old behavior.

## Alternatives Considered

### A standalone primitive (a named changelist distinct from branches)

Model each stream as a new first-class object — a "scene"/"stem"/changelist — separate from branches.

*Rejected because:* it would re-implement history, rebase, and durable storage that Lore branches already provide, and
it has no natural home for the continuous, durable backup trail that falls out of committing revisions onto a branch.

### Multiple instances over a shared store (worktree-style)

Use one instance per stream over a shared store, as Lore already supports.

*Rejected because:* each instance is a separate on-disk working tree, so streams are never co-resident; two streams
cannot overlay in the *same* file, and the developer pays a checkout and its management per stream.

### Selective staging at commit time

Keep one undifferentiated working set and separate streams by staging subsets just before each commit.

*Rejected because:* staging is file-granular and happens only at commit time; it preserves no grouping while work
continues, cannot park a stream, and offers no path to move a stream to another branch.

### A single git-stash-style stack

Add one anonymous stash stack to park work.

*Rejected because:* it is single and anonymous, not several named co-resident streams; it is not durable history, not
hunk-aware, and not portable across branches.

## Prior Art

- **Lore in Unreal Editor for Fortnite (the direct influence)** — Lore already backs UEFN with a single per-user
  backup branch the editor auto-commits every change onto (see Motivation). The continuous-capture model here comes
  from that practice; this proposal generalizes the one hidden branch into many managed, organizable, portable
  changesets.
- **Jujutsu** — the working copy is itself a commit, auto-snapshotted on every command (no staging area), and
  anonymous heads let many lines of work coexist. Crucially for this proposal, jj materializes **one** working-copy
  commit per directory and runs parallel streams through `jj workspace` — separate directories, its git-worktree
  equivalent — rather than overlaying several changes co-resident in one tree, which is exactly the gap changesets
  fill. jj also records conflicts as first-class objects and defers resolution — the same stance changesets take, a
  conflict against the ground riding as marked state on the changeset rather than blocking the attach. jj reaches
  continuous capture independently; it is convergent prior art, not the influence behind this proposal.
- **StGit / quilt** — maintain a stack of patches over a base and operate on the series as a unit (push/pop/refresh),
  the direct analog of attach/detach and committing the series as a unit.
- **Mercurial** — `shelve` parks work, bookmarks are lightweight movable pointers, and topics/evolve provide mutable,
  named lines of in-progress history.
- **Git** — `stash` parks work but is single and anonymous; worktrees give parallel checkouts but in separate
  directories; branches are mutually exclusive in one tree. Together they illustrate the gap this proposal targets.
- **Sapling** — easy commit stacks and restacking, showing routine movement of a line of history onto a new base.
- **Perforce pending changelists** — the closest existing model for grouping in-flight work. A *pending changelist*
  groups opened files with a description in one workspace; files start in the *default changelist* and are moved into
  numbered ones (`p4 reopen`), and `p4 shelve` stores a changelist's files server-side so work is backed up and other
  users can `p4 unshelve` it — directly parallel to the default changeset, organization-by-reassignment, and
  remote-synced sharing. It differs on the points this proposal needs: a file lives in exactly one changelist (no
  overlay of several streams in one tree), grouping is file-granular (not per-hunk), a changelist has no history of
  its own (no continuous per-stream backup trail), and it is tied to the workspace rather than being a portable unit
  you can land on any branch.

The continuous-capture design here comes from Lore's own UEFN usage above.

Worth borrowing from elsewhere: StGit's stack-as-unit operations, and Perforce's default/numbered-changelist
organization plus shelve-to-share.

Worth avoiding: git stash's single, anonymous, opaque model.

## Unresolved Questions

These are deliberately deferred to the implementation phase and settled by experimentation; none is a feasibility
blocker — each is a policy or default-behavior choice with a sensible starting default (e.g. assign a region-spanning
edit to the changeset it most overlaps, coalesce checkpoints, suppress checkpoint notifications, lock late).

- When a single edit spans regions owned by different attached changesets (or a changeset region and a ground region),
  is it split at the region boundary or assigned wholesale to one changeset?
- Are a changeset's checkpoint revisions visible in `lore history`, and how are they coalesced or garbage-collected
  (including the brief post-commit recovery window before a discarded changeset's line is collected)?
- What checkpoint cadence and coalescing keep revision volume bounded — debounce/save-driven vs. per edit, and do
  consecutive checkpoints coalesce amend-style?
- Should a checkpoint commit on a changeset suppress the notification fan-out that a regular branch advance triggers,
  so continuous checkpointing does not flood subscribers?
- When a remote-synced changeset is discarded, can the server fully expunge it, or only tombstone it?
- Should an exclusive lock default to being taken when a file is first edited in a changeset, or only when the
  changeset is committed onto a real branch (see successor-locks LEP)?
