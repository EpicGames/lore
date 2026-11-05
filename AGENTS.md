# AGENTS.md

Guidance for AI agents working in this repository.

## Development workflow

**You must load the `lore-development` skill (`.claude/skills/lore-development/SKILL.md`)
at the start of every session in this repository, before any other work, and follow it.**
This applies to every task: designing, writing, reviewing, building or testing code, and
answering questions about it. Load it again whenever the conversation is compacted or
resumed, and tell every subagent you brief to load it too.

The skill routes to the decisions and code standards that govern the change, and defines
the validation, review and submission steps. Work done without it doesn't meet this
repository's standards.

**You must review every change with the `review-change` skill
(`.claude/skills/review-change/SKILL.md`) before you submit it.** The review runs in a
fresh session, from the code, so it finds what the author's own reading misses.
Performance and allocations come first, and a fix must come with a test that fails
without it.

## Team workspace

When `context/team-workspace.md` exists, read it before non-trivial work and follow
it. It names the team's workspace for specs, plans, work items and decisions,
and how to reach it. Without it, work proceeds normally, and specs and plans stay in
`context/`.

## Where design decisions go

A spec plus plan is the whole design record for ordinary work. This project does not
generally use RFCs.

Four classes are different. A **larger architectural change**, anything touching a
**data format** or a **protocol**, and any **substantial new capability** are designed
in this repo, in public:

- **LEPs** — `docs/proposals/YYYY-MM-DD-<slug>.md`. Required per `GOVERNANCE.md`;
  minimum two-week discussion, and **accepted before implementation begins**.
- **ADRs** — `docs/developing/decisions/<NNNNN>-<slug>.md`, for rulings below the
  LEP bar.

If you are about to write code in one of those four classes, the proposal comes
first — not afterwards to describe what was built. The LEP bar is in `GOVERNANCE.md`
and `CONTRIBUTING.md`.

## Governance sign-off is a GitHub gate

The issue → PR → two-Maintainer-approval flow in `GOVERNANCE.md` and
`CONTRIBUTING.md` governs public contributions arriving through GitHub. It is not a
per-revision gate on work in Lore — a Lore branch is reviewed through its change
request.

Binding either way: an accepted LEP before implementation for the four classes
above, the Code of Conduct, and the security policy.
