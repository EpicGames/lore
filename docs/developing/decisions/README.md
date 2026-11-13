# Architectural decision records

Documents describing the various architectural decisions that have been made during the course of Lore development and the reasoning behind them.

## When to write one

A new ADR is retrospective. It records why an architectural decision was made, so that later contributors don't relitigate it. Write it with the code that makes the decision concrete, or after it. An ADR is never a proposal: to align on a change before it merges, write an [LEP](../../proposals/README.md).

Once an ADR is accepted, new code follows it. A change that departs from an accepted ADR comes with a new ADR that supersedes it, in the same change.

## Format

ADRs are append-only and numbered sequentially. Filename pattern: `NNNNN-<slug>.md`. Required structure: Status, Context, Decision, Consequences.

## Suggested starting points

- **Documenting a new Architectural Decision?** Start at the [doc-standards walkthrough](../doc-standards/writing-a-doc.md).
- [ADR template](adr-template.md). Copy this when starting a new ADR.

See [docs/README.md](../README.md) for the full docs structure.
