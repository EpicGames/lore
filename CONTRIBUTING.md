# Contributing to Lore

Lore is an open source project by Epic Games, and we welcome contributions from anyone.

Before you begin, read our [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

We welcome code contributions — bug fixes, features, performance improvements. The [roadmap](docs/roadmap.md) shows the big-rock themes we're working toward — a good place to find where help is wanted. There are also plenty of ways to help beyond writing code:

- **Report a bug** — open a GitHub Issue with steps to reproduce
- **Request a feature** — post in [#feature-requests on Discord](https://discord.gg/QYbNFVFv) or open a GitHub Issue
- **Improve documentation** — fix typos, clarify explanations, add examples
- **Triage issues** — help reproduce bugs, add labels, confirm scope
- **Review pull requests** — read the code, test it, leave thoughtful feedback

## Getting started

### Prerequisites

You need the following tools installed:

- **Rust** — install via [rustup](https://rustup.rs/). Lore requires the stable toolchain plus nightly for formatting.
- **Python 3.13+** — required for smoke tests, with [uv](https://github.com/astral-sh/uv) for dependency management.

Install the Rust toolchains:

```sh
rustup toolchain install stable
rustup toolchain install nightly
```

Optionally, install [pre-commit](https://pre-commit.com/) to run linting automatically on each commit. CI enforces the same checks, so this is a convenience rather than a requirement:

```sh
uv tool install pre-commit
pre-commit install
```

### Build and test

```sh
cargo build
cargo test
```

To run the same lint and format checks that CI enforces:

```sh
cargo +nightly fmt --all
cargo clippy --all-targets -- -D warnings --no-deps
```

For smoke tests:

```sh
uv run pytest
```

## Before you code

For anything beyond a trivial fix, **open a GitHub Issue before writing code:**

- Architectural decisions in a revision control system ripple widely. A brief discussion can save significant rework.
- A maintainer may already be working on the same problem.
- The change may be intentionally out of scope.

Wait for a maintainer to weigh in before investing significant effort.

For large or cross-cutting changes, check whether a [Lore Enhancement Proposal](#lore-enhancement-proposals) is required before any code is written.

## Development workflow

### Code standards

Read the full code standards in [`docs/developing/code-standards/`](docs/developing/code-standards/) before writing your first line.

### Formatting and linting

These are enforced by CI and must pass before any PR is merged:

```sh
cargo +nightly fmt --all        # auto-format; run before committing
cargo clippy --all-targets -- -D warnings --no-deps  # zero warnings required
```

The pre-commit hooks run these automatically if you've installed them.

### Commit messages

Write commit messages in the imperative: "Add sparse sync for large repositories", not "Added sparse sync". Keep the subject line under 72 characters. Include a body when the why isn't obvious from the subject alone.

Reference the issue your change addresses:

```text
Fix branch listing when remote has zero refs

When loreserver returns an empty refs list, the client panicked
instead of producing an empty result. Add a guard and a test case.

Fixes #123
```

### DCO sign-off

Every commit must include a `Signed-off-by:` line. This certifies that your contribution complies with the [Developer Certificate of Origin](DCO) — a lightweight declaration that you have the right to submit the change under the MIT license.

Add the sign-off automatically with:

```sh
git commit -s -m "Your commit message"
```

This appends:

```text
Signed-off-by: Your Name <you@example.com>
```

Commits without a sign-off will not be merged.

Because the MIT license doesn't include an express patent grant, the DCO sign-off also carries an affirmation about patents. By signing off on your commit, you affirm that, to the best of your knowledge, your contribution doesn't infringe any patents you are aware of. Providing your sign-off is a representation about your knowledge, not a grant of any patent license.

## Submitting a pull request

1. Fork the repository.
2. Make your changes and ensure all tests pass and linting is clean.
3. Add or update tests for the behavior you're changing.
4. Open a pull request targeting `main`. The PR description should cover:
   - **What:** what does this PR do?
   - **Why:** what problem does it solve? Link to the GitHub Issue.
   - **How:** a brief description of your approach, especially if non-obvious.
   - **Testing:** how did you verify the change works?
5. Every commit must include `Signed-off-by:` as described above.

Keep PRs focused. One logical change per PR.

## Review process

Expect at least one round of feedback before a PR is merged.

A PR requires **two approvals** from Maintainers to merge.

Address each piece of feedback, then re-request review. Maintainer decisions are final. If feedback isn't clear, ask.

## Lore Enhancement Proposals

An LEP is required before implementing any change that affects:

- The Lore wire protocol or on-disk format
- Public APIs (`lore` CLI commands, `lore-capi`, language bindings)
- Cross-cutting features that span multiple subsystems
- Any breaking change

For everything else, an Issue and PR are sufficient.

LEPs live in [`docs/proposals/`](docs/proposals/). Copy [`docs/proposals/lep-template.md`](docs/proposals/lep-template.md) to a new file named `YYYY-MM-DD-your-feature.md`, fill it out, and open a pull request. The LEP must be accepted before implementation begins. See the [LEP README](docs/proposals/README.md) for the full lifecycle and writing guidelines.

## Documentation

Documentation changes follow the same PR process as code. Read Lore's documentation standards in [`docs/developing/doc-standards/`](docs/developing/doc-standards/) before making significant doc changes.

Three linters keep docs consistent. See the [doc-standards tooling guide](docs/developing/doc-standards/tools/README.md) for install and configuration, then run them before opening a PR:

```sh
vale <file>                   # Lore house style
npx markdownlint-cli2 <file>  # Markdown formatting
lychee --config docs/developing/doc-standards/tools/lychee/lychee.toml docs/
# Run lychee across the whole tree, not just changed files: a moved or
# renamed page can break links in docs you didn't touch.
```

## Community

Get help on [Discord](https://discord.gg/QYbNFVFv):

| Channel | Purpose |
| --- | --- |
| `#announcements` | Project news and releases |
| `#welcome` | Introductions — say hello |
| `#general` | General Lore discussion |
| `#support-requests` | Help with using Lore |
| `#feature-requests` | Ideas for future development |
| `#show-and-tell` | Share what you've built with Lore |
| `#off-topic` | Everything else |

For longer-form design discussions, open a GitHub Issue or bring the conversation to `#general` on [Discord](https://discord.gg/QYbNFVFv).

Issues tagged [`good-first-issue`](https://github.com/EpicGames/lore/labels/good-first-issue) are a good starting point if you're new to the codebase. If you're unsure where to begin, ask in `#support-requests`.

## AI assistance policy

Contributions that used AI tools (GitHub Copilot, Claude, etc.) are welcome. When you do, you must:

- **Disclose** which AI tools you used in the PR description.
- **Review and test** all AI-generated code. You remain fully responsible for its correctness, security, and license compliance.
- **Write your own PR description** — AI-generated PR descriptions are not accepted.

For security-sensitive changes (cryptography, authentication, authorization), disclose AI involvement early and expect closer review.

## Legal

### License

Lore is licensed under the [MIT License](LICENSE). By submitting a pull request, you agree that your contribution may be used under that license.

All commits must include a `Signed-off-by:` line as described in the [DCO sign-off](#dco-sign-off) section. This confirms your agreement with the [Developer Certificate of Origin](DCO).

### Copyright headers

Preserve your own copyright on the code you contribute. For new files, add a copyright line and an SPDX identifier at the top:

```text
// Copyright 2026 Jane Smith
// SPDX-License-Identifier: MIT
```

When modifying existing files, leave Epic's copyright line in place and add your own below it.

### License compatibility

Contributed code must not introduce dependencies licensed under the GPL, LGPL, or AGPL. Their copyleft terms are incompatible with Lore's MIT license.
