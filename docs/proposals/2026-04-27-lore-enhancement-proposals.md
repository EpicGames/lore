---
lep: 2026-04-27-lore-enhancement-proposals
title: Lore Enhancement Proposals
authors:
  - raghav.narula
status: Approved
created: 2026-04-27
updated: 2026-04-28
discussion: https://crowd.urc.internal.epicgames.net/Epic/URC/change-request/1684
---

# Lore Enhancement Proposals

## Summary

This proposal introduces Lore Enhancement Proposals (LEPs) — a public-facing, pre-implementation proposal document type for changes to the Lore wire protocol, on-disk format, public APIs, and major cross-cutting features. LEPs fill the gap between ADRs (which record narrow architecture decisions after the fact) and specs and plans (which assume the decision exists). The lifecycle stays lightweight: an LEP starts as Draft, then becomes Accepted or Rejected on merge. No provisional state, no FCP, no graduation track.

## Motivation

Lore tracks design intent in three artifact types today: ADRs in `docs/decisions/` capture narrow architecture decisions made during or after implementation, specs in `docs/specs/` and `context/specs/` describe what the system does, and plans in `context/plans/` sequence the work. None of these covers the moment between *we should change something* and *we have decided what to change*.

The gap matters most for changes that touch external surfaces — wire protocol, on-disk format, public APIs — where alignment with internal teams that depend on Lore and with external contributors must happen before code lands. Without a public proposal venue, large changes either start as private design conversations that exclude interested parties or skip the design debate entirely and surface as faits accomplis. ADRs record the outcome but not the alternatives weighed; specs assume the decision; plans sequence the work after the decision lands.

What's missing is a place where the rationale, the alternatives considered, and the breakage risks live before any code lands — discoverable by both internal teams and external contributors, and durable enough that future contributors do not unknowingly retread dead-ended proposals.

## Goals / Non-Goals

### Goals

- Provide a canonical public venue for pre-implementation alignment on changes to wire protocol, on-disk format, public APIs, and major cross-cutting features.
- Keep the proposal lifecycle lightweight enough that authors actually use it: Draft, then Accepted or Rejected, with no provisional, deferred, withdrawn, or superseded limbo states.
- Force every proposal touching external surface to explicitly answer four questions: what alternatives did the author consider, what breaks for existing repos and clients, how does the rollout work, and what security and privacy implications follow.
- Make Lore's evolution legible to external contributors. A reader of `docs/proposals/` understands the design landscape and can submit a new LEP without first asking a maintainer for permission.

### Non-Goals

- Replace ADRs. Narrow architecture decisions made during or after implementation continue to land in `docs/decisions/`.
- Replace specs and plans. Implementation detail and execution sequencing stay where they are. LEPs stop at the design contract.
- Introduce governance machinery. No Steering Council, no FCP, no Alpha/Beta/GA graduation, no rfcbot.
- Own test plans. Testing detail belongs in the downstream spec or plan.

## Proposed Design

The LEP system has three pieces.

**A template at `docs/proposals/lep-template.md`** that authors copy. The template carries YAML frontmatter (lep, title, authors, status, created, updated, discussion, plus optional replaces and superseded-by) and 14 body sections in fixed order: Summary, Motivation, Goals/Non-Goals, Proposed Design, Compatibility (split into Wire format, Client/server protocols, On-disk format, and CLI and public API sub-sections), Non-Functional Considerations, Migration Plan, Security Considerations, Privacy Considerations, Risks and Assumptions, Drawbacks, Alternatives Considered, Prior Art, and Unresolved Questions. Authors fill the template and submit a CR.

**A directory `docs/proposals/`** indexed from `docs/README.md` and published via the existing mkdocs site. Filenames follow `YYYY-MM-DD-kebab-name.md`. Both Accepted and Rejected LEPs stay in-tree as historical record.

**Two skills, `/write-lep` and `/review-lep`**, that support authoring and reviewing while keeping the artifact human-readable. `/write-lep` is a how-to guide for drafting against the template, applying the per-section and cross-cutting rules from this README. `/review-lep` reads a draft and reports findings in three buckets — required fixes, recommended, optional — covering structural completeness, citation discipline (each factual claim carries either a public link or inline detail), and rationale quality (Goal and Motivation coverage in Proposed Design, `N/A` consistency with the rest of the proposal, load-bearing Risks and Drawbacks, substantive Alternatives, and active voice).

The lifecycle uses three states. An LEP starts as Draft. The CR review carries the discussion. On merge, the file's `status` field reflects the outcome — Accepted or Rejected. Withdrawn maps to closing the CR before merge. Superseded maps to the `superseded-by` frontmatter field on the older LEP. No FCP, no provisional state, no graduation track.

LEPs and ADRs co-exist with a clean split. An LEP debates a proposal before implementation. An ADR records the narrow architecture decisions made during implementation, linking back to the parent LEP when relevant. A change that does not touch wire, on-disk, or public-API surfaces and does not span subsystems skips the LEP and goes straight to an ADR.

Several content sections borrow specific patterns from outside Lore. Compatibility splits into four sub-sections — wire, client/server protocols, on-disk, and CLI and public API — applying [Swift Evolution's](https://www.swift.org/swift-evolution/) source-and-ABI compatibility split to Lore's four external surfaces. Migration Plan, when non-`N/A`, uses named transition phases following the model of git's [`hash-function-transition.adoc`](https://github.com/git/git/blob/master/Documentation/technical/hash-function-transition.adoc) (e.g. dark launch, early transition, late transition) with explicit per-phase read/write compatibility matrices. Security Considerations and Privacy Considerations never read bare `N/A`: a proposal with no implications must say so explicitly and explain why, following the [IETF RFC 7322](https://www.rfc-editor.org/rfc/rfc7322) mandatory-Security-Considerations pattern.

## Compatibility

- **Wire format:** N/A — this proposal adds documentation and authoring/review tooling. No message encoding changes.
- **Client/server protocols:** N/A — same. No RPC, message type, or authentication flow changes.
- **On-disk format:** N/A — same. The on-disk format does not change.
- **CLI and public API:** N/A — `/write-lep` and `/review-lep` run inside the editor and agent harness and do not extend the `lore` CLI, `lore-capi`, or JS-binding surfaces.

## Non-Functional Considerations

This proposal adds documentation and authoring/review tooling. The LEP system has no runtime presence in Lore client or server code, so the four core non-functional properties carry no impact:

- **Concurrency:** N/A — no runtime code path. LEP authoring and review run interactively in an editor or agent harness, well outside any Lore concurrent operation.
- **Memory:** N/A — LEPs are markdown files measured in kilobytes. No buffering or sparse-data-structure concerns apply.
- **Statelessness:** N/A — the skills hold no state across invocations beyond what the user types. The LEP system introduces no library- or process-level state.
- **Determinism:** N/A — Lore history is unaffected. LEP files are committed like any other doc.

## Migration Plan

N/A — this proposal adds new artifacts and tooling alongside existing ones. No data, no protocol, and no public surface changes, so nothing requires migration.

## Security Considerations

LEPs are public documents committed to the Lore source tree. Authors must not include secrets, credentials, internal hostnames, or other sensitive material in an LEP body or frontmatter — the same standard that already applies to any other commit. The LEP system itself introduces no new trust boundary, no new data path, and no new attack surface in the Lore client or server.

## Privacy Considerations

LEPs are public, so the `authors` frontmatter field exposes contributor identifiers to anyone reading the docs site. This matches existing practice for ADRs and commits. Authors choose what identifier to use — display name, username, or alias. The LEP system introduces no new collection, processing, or sharing of user data.

## Risks and Assumptions

**Assumptions** — premises that, if wrong, invalidate the proposal.

- **Assumption:** Maintainers and reviewers engage with LEPs rather than routing significant proposals around them through direct CRs. *Invalidated if:* maintainers consistently approve protocol or format CRs without an accompanying LEP, in which case the venue offers no actual alignment and becomes documentation theatre.

**Risks** — things that could go wrong even if the assumptions hold.

- **Risk:** LEPs become bureaucratic gatekeeping — every change demands one, even when an ADR or direct CR would do. *Mitigation:* the LEP/ADR split keeps narrow architecture decisions on the ADR path, and the skill guidance scopes LEPs to proposals that touch external surfaces or span subsystems, not every change.

## Drawbacks

- One additional artifact type for maintainers to track and review, with judgment overhead at the boundary between "narrow ADR" and "small LEP" that authors must apply.
- The lightweight lifecycle (no FCP, no graduation) puts more weight on individual reviewer judgment than a more formal process would, making cross-LEP consistency depend on maintainer discipline rather than tooling.

## Alternatives Considered

### Keep using ADRs for everything

Continue using `docs/decisions/` for both narrow architecture decisions and larger proposals, expanding the ADR template's Consequences section as needed to cover compatibility and migration concerns.

*Rejected because:* ADRs explicitly record decisions already made and do not serve as a forum for proposals under debate. Stretching ADRs to cover pre-decision proposals dilutes their meaning, and the ADR template lacks the forcing functions — split Compatibility, Migration Plan, Security and Privacy Considerations, Risks and Assumptions — that protocol and format proposals need.

### Adopt a heavyweight PEP/KEP-style process

Introduce a multi-state lifecycle (Draft, Provisional, Accepted, Final, Withdrawn, Superseded), a graduation track (Alpha/Beta/GA), and accompanying governance (Steering Council, FCP machinery, rfcbot-style automation).

*Rejected because:* the governance overhead presupposes a project size and contributor pattern Lore does not have, and the graduation track presupposes a runtime feature-gate model Lore does not run. The lifecycle states beyond Draft/Accepted/Rejected add ceremony without resolving real ambiguity for Lore changes.

### Repurpose `context/rfcs/`

Use the existing empty `context/rfcs/` folder for proposals, alongside `context/specs/` and `context/plans/`, instead of creating `docs/proposals/`.

*Rejected because:* `context/` is not part of the published docs site, so external contributors and downstream consumers would not discover proposals there. `docs/proposals/` sits alongside ADRs and specs in the published navigation and matches the intended audience — public, durable, indexed.

## Prior Art

Several enhancement-proposal processes from major projects shaped the LEP design without wholesale adoption of any one. The [Rust RFC process](https://github.com/rust-lang/rfcs) supplied the basic shape — Summary, Motivation, Drawbacks, Rationale and alternatives, Prior art, Unresolved questions. [Python PEPs](https://peps.python.org/) contributed Backwards Compatibility and Security Implications as forcing functions. [Kubernetes KEPs](https://github.com/kubernetes/enhancements) contributed explicit Goals/Non-Goals and the standalone Drawbacks section; LEPs reject KEP's Alpha/Beta/GA graduation track and Production Readiness Review questionnaire as Kubernetes-specific. [Swift Evolution](https://www.swift.org/swift-evolution/) contributed the Source/ABI compatibility split, applied here to Lore's four external surfaces (wire, client/server protocols, on-disk, CLI and public API). [OpenJDK JEPs](https://openjdk.org/jeps/0) contributed Risks and Assumptions. Apache Cassandra CEPs contributed the "Compatibility, Deprecation, and Migration Plan" structure. Git's [`hash-function-transition.adoc`](https://github.com/git/git/blob/master/Documentation/technical/hash-function-transition.adoc) contributed the named-transition-phases pattern for Migration Plan. [IETF RFC 7322](https://www.rfc-editor.org/rfc/rfc7322) contributed the mandatory Security Considerations gate. Jujutsu's design-doc process contributed the lightweight comment-window review model that LEPs follow without further machinery.

## Unresolved Questions

- **LEP numbering source.** The current convention uses `YYYY-MM-DD-kebab-name`. Alternatives include tying the `lep` field to an associated CR or JIRA ticket number (KEP-style breadcrumb) or using a sequential counter. Revisit if cross-references between LEPs become awkward.
- **External contributors without CR write access.** The CR-based discussion model assumes the author can open a CR. An alternative discussion venue (forum, email, sponsored CR) may be needed for outside contributions.
- **Mkdocs publication scope.** Should rejected LEPs appear on the published docs site or only accepted ones? Both have precedent — PEPs and Cassandra CEPs publish rejected; Rust RFCs only list accepted ones.
- **Retroactive ADR migration.** Some current ADRs (e.g. `00015-compression-algorithm-selection`, `00009-fragment-max-size`) are arguably LEP-shaped. Decide whether to retroactively migrate any.
- **Hand-off contract from Accepted LEP to downstream spec or plan.** When an accepted LEP triggers a spec, define what content carries over and what new content the spec adds.
