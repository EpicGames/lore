<!-- Language: use the active voice and plain, literal words. No filler, metaphors, or idioms. Keep it concise. -->

## Motivation

<!-- Explain the intent behind this change: what problem it solves or what outcome it enables. Reviewers use this to check the change does what it claims. State the problem the change solves, or link the issue or LEP that states it. When the change meets the LEP bar, link its LEP. -->

Closes #

## Summary

<!-- Explain how the change meets the motivation, with only the detail needed for that. Don't describe every code change. -->

## Test Plan

<!-- State how you proved the change works, as repeatable steps. CI steps aren't enough. Name the tests that cover each behavior the change adds or changes, and for a fix, the test that fails without it. Name any provider or platform the tests don't run on, and why. For a change to async code, give the measurements async-futures.md section 10 puts in the commit message; for a change to a hot path, give measurements against the base. -->

## Checklist

- [ ] The description states the problem, or links the issue or LEP that does
- [ ] Commit messages use the imperative mood ("Add sparse sync", not "Added sparse sync"), keep the subject under 72 characters, explain the reason in a body when the subject doesn't, and reference the issue (`Fixes #123`)
- [ ] Every commit carries a `Signed-off-by:` line (DCO -- see [CONTRIBUTING.md](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md#dco-sign-off))
- [ ] `cargo +nightly fmt --all` produces no changes
- [ ] `cargo clippy --all-targets -- -D warnings --no-deps` passes
- [ ] `cargo test --release` passes
- [ ] `uv run pytest scripts/test/ --lore-client-binary=release --lore-server-binary=release` passes
- [ ] Each behavior the change adds or changes has a test, and a fix has a test that fails without it ([testing.md](https://github.com/EpicGames/lore/blob/main/docs/developing/code-standards/testing.md#what-a-change-tests))
- [ ] The change was reviewed against [code-review.md](https://github.com/EpicGames/lore/blob/main/docs/developing/code-standards/code-review.md) before opening this PR
- [ ] A change to async code reports its future sizes against the base, and its allocation counts when it adds or removes a box, in its commit message ([async-futures.md](https://github.com/EpicGames/lore/blob/main/docs/developing/code-standards/async-futures.md#10-measurement-and-tests))
- [ ] A change to shipped behavior adds a note to [`docs/release-notes/`](https://github.com/EpicGames/lore/blob/main/docs/release-notes/README.md)
- [ ] Docs under `docs/` updated where behavior changed
- [ ] If this change affects the wire protocol, the on-disk format or a public API, adds new functionality, spans multiple subsystems, or is a breaking change, its [LEP](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md#lore-enhancement-proposals) has been accepted
- [ ] If this change makes a decision that is hard to reverse or establishes a pattern, it adds an [ADR](https://github.com/EpicGames/lore/blob/main/docs/developing/decisions/README.md)
