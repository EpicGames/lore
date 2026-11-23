---
status: accepted
date: 2026-11-20
---

# ADR-00043: Every mount of a linked repository pins one revision

## Context and Problem Statement

A parent can link one repository at several paths, each mount exposing another folder of it. The
pin is stored per mount. Only `link add` kept the mounts of one repository at one revision: a
commit through one mount, `link update` and unstaging an update moved that mount alone, so a parent
revision could pin one repository at several revisions.

Should the mounts of one repository share their pin, or each keep their own?

## Decision Drivers

- The mounts of one repository are views of one tree: a parent revision should name one revision
  of it, so a sync, a clone and a merge see one tree.
- Revisions already written with mounts that disagree must stay readable.
- No change to the stored format or the protocol.

## Considered Options

- One pin per repository: every change to a mount's pin moves every mount of the repository.
- Independent mounts: each mount keeps its own pin.

## Decision Outcome

Chosen option: "one pin per repository", because it makes a parent revision name one tree of each
linked repository without a format change. Independent pins can come later as an explicit flag.

- A commit, `link update` and unstaging an update move every mount of the repository.
- `link add` of a repository already mounted takes its mounts' pin, branch and flags, and refuses
  others.
- A save refuses the mounts of a repository it changed where they disagree on branch or flags, and,
  in a committed revision, on the revision, failing with `LinkPinsDiverge` (FFI code 53).

### Consequences

- Good, because every parent revision written from now on pins each linked repository once.
- Good, because a revision written before, whose mounts disagree, stays readable until a change
  touches that repository; `lore link update` then moves its mounts to one revision.
- Bad, because mounts of one repository can no longer follow different branches, carry different
  flags or pin different revisions.
- Bad, because changes staged under two mounts of one repository are committed once only where they
  stage the same revision of it, and refused otherwise.
