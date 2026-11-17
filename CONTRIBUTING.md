# Contributing to Lore

Lore is an open source project by Epic Games, and we welcome contributions from anyone. This page sets out what a contribution must meet to be accepted, the steps it goes through, and your legal responsibilities as a contributor. Who holds which role and who decides is in [`GOVERNANCE.md`](GOVERNANCE.md).

Before you begin, read our [Code of Conduct](CODE_OF_CONDUCT.md). To set up a development environment, build Lore and run its checks, follow [Set up, build and test Lore](docs/developing/getting-started.md).

## Ways to contribute

We welcome code contributions — bug fixes, features, performance improvements. The [roadmap](docs/roadmap.md) shows the big-rock themes we're working toward — a good place to find where help is wanted. There are also plenty of ways to help beyond writing code:

- **Report a bug** — open a GitHub Issue with steps to reproduce
- **Request a feature** — post in [#feature-requests on Discord](https://discord.gg/E4SFJKRPbg) or open a GitHub Issue
- **Improve documentation** — fix typos, clarify explanations, add examples
- **Triage issues** — help reproduce bugs, add labels, confirm scope
- **Review pull requests** — read the code, test it, leave thoughtful feedback

## Issues or pull requests

- **Found a problem you can't fix, or don't want to?** Open a GitHub Issue that describes it, so that someone else can pick it up.
- **Want to fix it yourself?** Send a pull request. Its description states the problem it solves, so it needs no issue first.
- **Does the change meet the [LEP bar](#lore-enhancement-proposals)?** Write the LEP first. The LEP states the problem, and the implementation can't be merged until the LEP is accepted.

## Workflow

1. **Write the LEP, if the change needs one.** Discuss it until you and the maintainers agree on the approach, and wait for that agreement before investing significant effort. For architectural questions, use [Discord](https://discord.gg/E4SFJKRPbg).
2. **Make the change** in a fork, meeting the [acceptance policies](#acceptance-policies).
3. **Open a pull request** targeting `main`, and fill in the [PR template](.github/PULL_REQUEST_TEMPLATE.md). Its description states the problem the change solves, or links the issue or LEP that does.
4. **Review.** See [Review](#review).
5. **Merge.** A Maintainer merges the pull request.

## Acceptance policies

A pull request is accepted only when it meets all of these:

- **Code standards.** The change follows the [code standards](docs/developing/code-standards/); read them before you write your first line. Performance comes first, including the number and size of allocations: see the [performance checks](docs/developing/code-standards/code-review.md#performance). Your change is reviewed against [code-review.md](docs/developing/code-standards/code-review.md), so read it before you write the change.
- **Tests.** Each behavior the change adds or changes has a test, as [What a change tests](docs/developing/code-standards/testing.md#what-a-change-tests) describes. A bug fix comes with a test that fails without it.
- **Checks.** All CI checks pass. Maintainers may also ask for changes based on internal CI checks.
- **Release notes.** A change to what Lore does (client, server, API, protocol or stored data) adds a release note to [`docs/release-notes/`](docs/release-notes/README.md) rather than editing `docs/release-notes.md`.
- **Documentation.** Docs change with the behavior they describe, and follow the [documentation standards](docs/developing/doc-standards/). Documentation changes follow the same pull request process as code.
- **LEP and ADR.** A change that meets the [LEP bar](#lore-enhancement-proposals) has an accepted LEP, and a change that meets the [ADR bar](#architectural-decision-records) adds its ADR.
- **Sign-off.** Every commit carries a [DCO sign-off](#dco-sign-off).
- **Focus.** One logical change per pull request.

AI tools follow [`AGENTS.md`](AGENTS.md) and the skills in `.claude/skills/`, which route to the same standards.

## Review

Expect at least one round of feedback before a pull request is merged.

A pull request needs **one approval** from a Maintainer to merge. An objection blocks it; if discussion can't resolve the objection, the Core Maintainers make the final decision.

Address each piece of feedback, then request review again. If feedback isn't clear, ask.

## Lore Enhancement Proposals

An accepted LEP is required before merging any change that affects:

- The Lore wire protocol or on-disk format
- Public APIs (`lore` CLI commands, `lore-capi`, language bindings)
- New functionality
- Cross-cutting features that span multiple subsystems
- Any breaking change

For everything else, no LEP is needed. When a change meets this bar, an issue or PR description doesn't replace the LEP.

LEPs live in [`docs/proposals/`](docs/proposals/). Copy [`docs/proposals/lep-template.md`](docs/proposals/lep-template.md) to a new file named `YYYY-MM-DD-your-feature.md`, fill it out, and open a pull request. LEPs are discussed in public. The LEP must be accepted before its implementation is merged; implementation work may start while the LEP is under discussion. An accepted LEP is binding: the implementation must match it. See the [LEP README](docs/proposals/README.md) for the full lifecycle and writing guidelines.

## Architectural decision records

When a change makes a decision that is hard to reverse, or that establishes a pattern later code should follow, record it as an ADR in [`docs/developing/decisions/`](docs/developing/decisions/README.md). Write the ADR with the code that makes the decision concrete, or after it. When a change meets this bar, the ADR is required, and an issue or PR description doesn't replace it.

Accepted ADRs bind new code. A change that departs from one comes with a new ADR that supersedes it, in the same pull request. See the [ADR README](docs/developing/decisions/README.md) for when to write one and how.

## AI assistance policy

Contributions that used AI tools (GitHub Copilot, Claude, etc.) are welcome. When you do, you must:

- **Disclose** which AI tools you used in the PR description.
- **Review and test** all AI-generated code. You remain fully responsible for its correctness, security, and license compliance.

For security-sensitive changes (cryptography, authentication, authorization), disclose AI involvement early and expect closer review.

## Legal

### DCO sign-off

Every commit must include a `Signed-off-by:` line. This certifies that your contribution complies with the [Developer Certificate of Origin](DCO) — a lightweight declaration that you have the right to submit the change under the MIT license. [Set up, build and test Lore](docs/developing/getting-started.md#sign-off-commits) shows how to add it.

Commits without a sign-off will not be merged.

Because the MIT license doesn't include an express patent grant, the DCO sign-off also carries an affirmation about patents. By signing off on your commit, you affirm that, to the best of your knowledge, your contribution doesn't infringe any patents you are aware of. Providing your sign-off is a representation about your knowledge, not a grant of any patent license.

### License

Lore is licensed under the [MIT License](LICENSE). By submitting a pull request, you agree that your contribution may be used under that license.

### Copyright headers

Preserve your own copyright on the code you contribute. For new files, add a copyright line and an SPDX identifier at the top:

```text
// Copyright 2026 Jane Smith
// SPDX-License-Identifier: MIT
```

When modifying existing files, leave Epic's copyright line in place and add your own below it.

### License compatibility

Contributed code must not introduce dependencies licensed under the GPL, LGPL, or AGPL. Their copyleft terms are incompatible with Lore's MIT license.

## Community

Get help on [Discord](https://discord.gg/E4SFJKRPbg):

| Channel | Purpose |
| --- | --- |
| `#announcements` | Project news and releases |
| `#welcome` | Introductions — say hello |
| `#general` | General Lore discussion |
| `#support-requests` | Help with using Lore |
| `#feature-requests` | Ideas for future development |
| `#show-and-tell` | Share what you've built with Lore |
| `#off-topic` | Everything else |

For longer-form design discussions, open a GitHub Issue or bring the conversation to `#general` on [Discord](https://discord.gg/E4SFJKRPbg).

Issues tagged [`good-first-issue`](https://github.com/EpicGames/lore/labels/good-first-issue) are a good starting point if you're new to the codebase. If you're unsure where to begin, ask in `#support-requests`.
