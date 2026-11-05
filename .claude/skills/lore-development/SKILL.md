---
name: lore-development
description: Must be loaded before designing, writing, reviewing, building or testing code in the Lore repository (Rust crates, binaries, configuration, or tests), and before answering questions about it. Routes to the decisions and code standards that govern the change, defines the validation, review and submission steps, and points to the review procedure. Load it before the work, not after.
---

# Lore Development

## Guidance

The standards in `docs/developing/code-standards`, indexed in its `README.md`. You must read each standard a step names before taking that step, in full and in the current session. This skill names the standards and restates only the rules most often missed. Memory of an earlier reading is no substitute. When you report the work, name the standards you read.

- For design trade-offs, read and follow `docs/developing/code-standards/engineering-principles.md`.
- For error handling, read and follow `docs/developing/code-standards/errors.md`.
- For logging, read and follow `docs/developing/code-standards/logging.md`.
- For task spawning, read and follow `docs/developing/code-standards/tasks.md`.
- For async code, read and follow `docs/developing/code-standards/async-futures.md`.
- For tests, and for building and running them, read and follow `docs/developing/code-standards/testing.md`.
- For comments and documentation, read and follow `docs/developing/code-standards/comments.md`.
- For terms in code, comments, messages and docs, read and follow `docs/glossary.md` and the Lore vocabulary in `docs/developing/doc-standards/canon/language.md`.
- For the model a change must keep, read the sections of `docs/explanation/system-design.md` for the area it changes.
- For what the change is reviewed against, read `docs/developing/code-standards/code-review.md`. Its checks apply while writing, not only in review: completeness, concision, performance, re-use and testing.

## Team workspace

When `context/team-workspace.md` exists, follow it before non-trivial work. It describes how to find the specs, decisions and open problems that cover the work, how to track the work, and where to record decisions and knowledge.

## Before writing code

1. Search `docs/developing/decisions` for decisions about the work. Read those that apply, and follow them.
2. Read the standards in [Guidance](#guidance) that apply to the work.
3. Search the code for functions and types that already do what the work needs. Generalize one rather than add another beside it, unless the general form costs performance on a hot path, and say so in the new function's doc comment.

## While writing code

Follow the standards in [Guidance](#guidance) that apply to the code.

1. Run `cargo check` and `cargo +nightly fmt --all` after each meaningful edit.
2. Read `testing.md` before building or running any test, and build and run tests as it describes.
3. Write the tests with the code, as `testing.md` describes in *What a change tests*. A fix must start with a test that fails without it.
4. Put Rust unit tests in the crate's `tests/unit/`, not an inline `#[cfg(test)]` module, and give a test its own `tests/*.rs` binary only when it needs its own process. Reach a private item only through the crate's `test-util` feature, usually with `#[lore_macro::test_pub]`, as `testing.md` describes.
5. Validate the finished work with the full unit test suite, run as `testing.md` describes, and `cargo clippy --all-targets -- -D warnings --no-deps`.
6. After building, run `lore status --scan` over the whole tree. A build rewrites tracked files such as `Cargo.lock`, and CI fails a change that leaves them out.
7. For a change to async code, read `async-futures.md` §10. You must measure the change against the base as it describes, and report the results in the commit message.

## After writing code

Read the standards in [Guidance](#guidance) that apply to the finished work again, and check the work against them.

1. Record an architectural decision that would take more than a day to reverse as an ADR in `docs/developing/decisions`, following `adr-template.md` and that directory's `README.md`. Set a superseded ADR's `status` field to `superseded by [ADR-NNNNN](NNNNN-slug.md)`.
2. Update any page in `docs/explanation`, `docs/how-to`, or `docs/tutorials` that the change makes stale.
3. Add a release note for every change to shipped behaviour: a file in `docs/release-notes/`, as its `README.md` describes, rather than an entry in `docs/release-notes.md`. Only tests, CI, tooling, docs and behaviour-preserving refactors need none.
4. Check each claim in the comments and docs the change adds against the code, not against memory of writing it.
5. Review the change as below, then submit it.

## Reviewing a change

You must review every change with the `review-change` skill (`.claude/skills/review-change/SKILL.md`) before you submit it. It reviews the change in a fresh session as `docs/developing/code-standards/code-review.md` describes, fixes the findings that have one obvious fix, and reports the rest. Use the same skill to review another author's change.

## Submitting a change

1. Run all the tests: the full unit and smoke test suites, run as `docs/developing/code-standards/testing.md` describes, and `cargo clippy --all-targets -- -D warnings --no-deps`.
2. Verify the work by hand to prove it works. Run the changed behaviour and check the result. These steps become the CR's test plan.
3. Commit with a message that carries no authorship or tool attribution, such as a `Written with`, `Generated with` or `Co-Authored-By` line. Record a co-author as revision metadata instead, with `lore revision metadata set co-author "<name> <email>"` before `lore commit`.
4. Create the CR, with a description that follows `.crowd/change_request_template.md` and carries no such attribution either.

For a pull request on GitHub, follow `CONTRIBUTING.md` for steps 3 and 4 instead: sign off every commit, disclose the AI tools used, and leave the pull request description to the human author.

For a small update to a submitted CR, a full smoke test run isn't needed. CI runs the smoke tests.
