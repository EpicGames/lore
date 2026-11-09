# Summary

<!-- What does this change, and why? -->

Closes #

## Checklist

- [ ] Linked to an issue above, or explained why none applies
- [ ] Every commit carries a `Signed-off-by:` line (DCO -- see [CONTRIBUTING.md](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md#dco-sign-off))
- [ ] `cargo +nightly fmt --all` produces no changes
- [ ] `cargo clippy --all-targets -- -D warnings --no-deps` passes
- [ ] `cargo test --release` passes
- [ ] `uv run pytest scripts/test/` passes
- [ ] Each behavior the change adds or changes has a test, and a fix has a test that fails without it ([testing.md](https://github.com/EpicGames/lore/blob/main/docs/developing/code-standards/testing.md#what-a-change-tests))
- [ ] The change was reviewed against [code-review.md](https://github.com/EpicGames/lore/blob/main/docs/developing/code-standards/code-review.md) before opening this PR
- [ ] A change to async code reports its future sizes against the base, and its allocation counts when it adds or removes a box, in its commit message ([async-futures.md](https://github.com/EpicGames/lore/blob/main/docs/developing/code-standards/async-futures.md#10-measurement-and-tests))
- [ ] A change to shipped behavior adds a note to [`docs/release-notes/`](https://github.com/EpicGames/lore/blob/main/docs/release-notes/README.md)
- [ ] Docs under `docs/` updated where behavior changed
- [ ] If this change is large or cross-cutting, its [LEP](https://github.com/EpicGames/lore/blob/main/docs/proposals/README.md) has been accepted
