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

Two kinds of document live in this repo, in public, and they come at different times:

- **LEPs** — `docs/proposals/YYYY-MM-DD-<slug>.md`. An LEP aligns people on a change
  before it merges. A change needs one when it affects the **wire protocol or on-disk
  format**, **public APIs** (`lore` CLI commands, `lore-capi`, language bindings),
  **cross-cutting features that span multiple subsystems**, or is **any breaking
  change**. The LEP bar is in `GOVERNANCE.md` and `CONTRIBUTING.md`. An LEP is discussed
  in public and **accepted before the implementation is merged**; implementation work
  may start while it is under discussion. Write it early, not afterwards to describe
  what was built.
- **ADRs** — `docs/developing/decisions/<NNNNN>-<slug>.md`. An ADR records why an
  architectural decision was made. Write a new ADR with the code that makes the decision
  concrete, or after it; it is never a proposal. Accepted ADRs bind new code: a change that
  departs from one comes with a new ADR that supersedes it, in the same change.

## Binding rules

The Code of Conduct and the security policy bind every change. A change that clears the
LEP bar also needs an accepted LEP before it merges.
