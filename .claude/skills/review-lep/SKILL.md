---
name: review-lep
description: Review a Lore Enhancement Proposal for structural completeness, citation discipline, and rationale quality. Reports findings categorized by severity. Read-only.
argument-hint: [path-to-lep]
---

# Review LEP

Review a Lore Enhancement Proposal in `docs/proposals/` and report findings categorized by severity. The skill is **read-only** — it does not edit the LEP. The author is responsible for applying any fixes.

The review has three phases:

1. **Structural** — deterministic check that all required sections and frontmatter fields are present and filled.
2. **Citations on claims** — every factual claim must reference a public source (ADR, code path in the Lore tree, benchmark, upstream doc) or carry its support inline.
3. **Rationale quality** — alternatives, drawbacks, and risks must be load-bearing rather than checkbox-filling.

## Arguments

- **No arguments**: The skill will list all `docs/proposals/*.md` files (excluding `README.md` and `lep-template.md`) and ask which to review.
- **`<path>`**: Path to the LEP to review, e.g. `/review-lep docs/proposals/2026-04-27-fragment-flag-reorganization.md`.

## Instructions

### Step 1: Locate the LEP

If a path was provided as an argument, use it. Otherwise list `docs/proposals/*.md` (excluding `README.md` and `lep-template.md`) and ask the user which to review.

If the path doesn't exist, report the error and stop.

### Step 2: Read the LEP, the README, and the template

Use the Read tool to read:

- The LEP under review.
- `docs/proposals/README.md` — per-section rules and cross-cutting rules (active voice, public citations, no fabrication).
- `docs/proposals/lep-template.md` — section structure and frontmatter shape.
- `docs/glossary.md` — the Lore terms the proposal must use in their defined sense.

If the LEP already has review comments, read them. Treat them as input to verify, and refer to a point a comment already makes rather than repeating it.

Phase 3 references rules from `docs/proposals/README.md` rather than restating them.

### Step 3: Phase 1 — Structural check

Deterministic. For each item, mark **PASS** or **FAIL**. Note the line number where applicable.

**Frontmatter:**

- All required fields present: `lep`, `title`, `authors`, `status`, `created`, `updated`.
- If `discussion` is set, it is a public link (for example a public pull request or issue). A link to an internal review, ticket, chat thread or wiki is a FAIL.
- `status` is one of `Draft`, `Accepted`, `Rejected`.
- `lep` value matches the filename (without `.md`).
- `created` and `updated` are valid `YYYY-MM-DD` dates.
- If `replaces` is set, the referenced LEP exists in `docs/proposals/`.
- If `superseded-by` is set, the referenced LEP exists in `docs/proposals/`.

**Body sections (all 14 must be present in order):**

1. Summary
2. Motivation
3. Goals / Non-Goals
4. Proposed Design
5. Compatibility (with four items: Wire format, Client/server protocols, On-disk format, CLI and public API)
6. Non-Functional Considerations
7. Migration Plan
8. Security Considerations
9. Privacy Considerations
10. Risks and Assumptions
11. Drawbacks
12. Alternatives Considered
13. Prior Art (may be omitted with explicit justification)
14. Unresolved Questions

**Content checks:**

- No section contains `{TODO}`, `{...}` placeholders, or other template scaffolding left from `lep-template.md`.
- Each Compatibility item (Wire format, Client/server protocols, On-disk format, CLI and public API) is filled or marked `N/A`. A reason after the dash is helpful but not required structurally. Whether `N/A` is correct given the rest of the proposal is checked in Phase 3.
- Migration Plan is filled or marked `N/A`. Phase 3 verifies consistency with Compatibility.
- Security Considerations is filled. `N/A` is **not** acceptable here — if there are no security implications, the section must say so explicitly and explain why.
- Privacy Considerations is filled. `N/A` is **not** acceptable here — if there are no privacy implications, the section must say so explicitly and explain why.
- Non-Functional Considerations is filled. The section addresses concurrency, memory, statelessness, and determinism — either confirming the proposal preserves each property or naming the impact and how the design accommodates it. Whether the response for each property is correct given the rest of the proposal is checked in Phase 3.
- If `status: Accepted`, the Unresolved Questions section is empty or contains only public references to follow-up work. Open questions in an Accepted LEP are a FAIL.

### Step 4: Phase 2 — Support for claims

LEPs are publicly consumable. Every factual claim must be supported in a way an external reader can verify or evaluate. There are two acceptable forms of support:

1. **A public citation** — a link to a public document, RFC, code path in the Lore tree, ADR, internals doc, or upstream project doc. The reader can follow the link.
2. **Inline detail** — the relevant data, code reference, paraphrased constraint, or excerpt captured in the LEP body. The reader sees the support without leaving the document.

**Internal-only links do NOT count as support.** Internal code reviews, JIRA tickets, Slack threads, internal wikis, and any other resource gated behind the company SSO are not visible to external readers. If a claim's only support is an internal link, flag it and require the author to either replace the link with a public citation or capture the relevant detail inline.

**Categories of claim that require support:**

- **Quantitative claims** — numbers, percentages, throughputs, sizes, latencies (e.g. "reduces storage by 40%", "compresses at 136 MiB/s"). Acceptable: benchmark table inline, link to a public benchmark or ADR with the data.
- **Claims about current Lore behavior** — how Lore works today (e.g. "fragment flags occupy bits 0-9", "the mutable store uses RocksDB"). Acceptable: link to the relevant code path or doc in the Lore tree, paraphrase of the behavior with file/function reference.
- **Claims about other systems** — Git, Mercurial, Perforce, Jujutsu, etc. (e.g. "Git's hash transition uses translation tables"). Acceptable: link to the upstream doc, paraphrase with concrete enough detail to reconstruct the claim.
- **Claims about decisions or constraints** — prior decisions or external requirements (e.g. "approach X was ruled out due to Y", "compliance requires per-tenant encryption"). Acceptable: paraphrased rationale captured inline, link to a public ADR or LEP.

**Categories of claim that do NOT require support:**

- The author's own design proposals — the LEP is the primary source for what it proposes.
- Genuinely common knowledge — "SHA-256 produces a 32-byte digest", "TCP is a stream protocol".
- Claims explicitly framed as assumptions in the Risks and Assumptions section — those need an invalidation condition, not a citation.

For each unsupported claim, record: section name, line number, the claim text, and whether the author needs a public citation or inline detail. For each claim supported only by an internal-only link, record the same plus the offending link.

### Step 5: Phase 3 — Rationale quality

LLM judgment. Each bullet names a rule and points to where it is defined; verify the LEP against that rule. `docs/proposals/README.md` is the source of truth — do not restate or substitute. Flag violations with section name, line number, and a one-line description of what to strengthen.

- **One decision per proposal** — see README cross-cutting rules. Flag a proposal whose parts have their own goals, risks, and compatibility surface, so that accepting one decides the others. Name the split, the order the parts depend on each other, and any part that belongs in an amendment to an accepted LEP (README `Lifecycle`). Required fix.
- **Design level** — see README `Proposed Design`. Walk Proposed Design, and flag implementation detail: names of functions, types, and files that aren't a compatibility surface, constants that aren't a stated limit or fixed by a format Compatibility names, struct ownership, and the order of steps inside a command. For each, say what moves to the spec and what property stays. Required fix when implementation detail makes up most of Proposed Design; recommended otherwise.
- **Lore terms** — see README cross-cutting rules. Flag a term the glossary defines that the proposal uses in another sense, and a Git or Perforce term `docs/developing/doc-standards/canon/language.md` maps to a Lore term.
- **Goal → Design coverage** — see README `Proposed Design`.
- **Motivation → Design coverage** — see README `Proposed Design`. Also flag design content that traces to neither a Motivation problem nor a Goal (over-scoped).
- **Motivation describes the problem, not the solution** — see README `Motivation`. Flag passages that prescribe *how* ("we require pagination", "we need read/write separation") instead of naming the user need or constraint ("lists can be very large and need navigation", "background operations must not block foreground requests"). Recommended fix.
- **Compatibility — N/A consistency** — for each sub-section marked `N/A`, walk Proposed Design, Migration Plan, Risks and Assumptions, and Drawbacks. If any concern there touches that surface, the `N/A` is contradicted — required fix. (Bare `N/A` without a reason is fine on its own; the test is whether the proposal's other content disagrees.)
- **Compatibility — surfaces beyond the four standard items** — see README `Compatibility` ("Add additional items for any other compatibility surface…"). If the proposal touches a surface (configuration formats, env vars, third-party library APIs, plugin interfaces, file paths treated as stable, etc.) and Compatibility omits it as a custom item, flag the gap.
- **Compatibility — concreteness** — for each non-`N/A` sub-section, verify the answer addresses the questions in the README (specific bits, specific commands, specific protocol fields). Hand-wavy answers ("there will be some impact") are recommended fixes.
- **Non-Functional — N/A consistency** — for each property dismissed as `N/A` or "preserved", walk Proposed Design, Risks and Assumptions, and Drawbacks. If any concern there touches that property, the dismissal is contradicted — required fix.
- **Non-Functional — concerns beyond the four standard items** — see README `Non-Functional Considerations`. Same logic as Compatibility's additional-surfaces check, with non-functional concerns such as latency, throughput, durability, ordering guarantees, backpressure, error budgets, observability.
- **Migration Plan** — see README `Migration Plan`. If any Compatibility sub-section flags a breaking change but Migration Plan is `N/A`, the `N/A` is contradicted — required fix. Otherwise verify named transition phases with read/write compatibility matrices, plus rollback signal and recoverable state. Hand-wavy phases or missing rollback are recommended fixes.
- **Risks and Assumptions structure** — see README `Risks and Assumptions`. Each Assumption must carry an `*invalidated if:*` clause; each Risk must carry a `*mitigation:*` clause. Mixed bullets without the two-list structure are recommended fixes.
- **Risks and Assumptions — load-bearing** — generic risks ("could have unexpected interactions", "performance might be worse") add no value.
- **Drawbacks shape** — see README `Drawbacks`. One sentence each; not re-statable as an Assumption (premise that could be wrong) or a Risk (failure mode); not generic hedges ("more code to maintain", "may require documentation updates").
- **Alternatives Considered** — see README `Alternatives Considered`. ≥2 alternatives, each with a concrete rejection reason. Strawmen and lazy rejections ("this would be harder") are recommended fixes; the status quo is a valid alternative if the rejection reason is concrete.
- **Goals / Non-Goals — toothless Non-Goals** — Non-Goals must narrow scope. Obvious things nobody would expect ("we're not solving world hunger") are recommended fixes.
- **Active voice** — see README cross-cutting rules. Flag true passive constructions and suggest active rewrites; past-participle adjectives are fine.

For each weakness, record: section name, line number where applicable, and a one-line description of what to strengthen.

### Step 6: Report

Output a structured report with three buckets:

**Required fixes** — must be addressed before the LEP can move to Accepted:

- Any structural FAIL.
- Any uncited quantitative claim.
- Empty Security Considerations.
- Open Unresolved Questions when `status: Accepted`.
- Any `N/A` in Compatibility, Non-Functional Considerations, or Migration Plan, and any property marked preserved, that the rest of the proposal contradicts.
- Independent decisions bundled in one proposal.
- A Proposed Design given mostly to implementation detail.

**Recommended** — should be addressed but not blocking:

- Uncited claims about Lore behavior or other systems.
- A Goal or Motivation problem the design doesn't address, and design content that traces to neither.
- A compatibility surface or non-functional concern the proposal touches but doesn't list.
- Implementation detail in an otherwise design-level section.
- Terms used in a sense other than the glossary's.
- Toothless Non-Goals.
- Hand-wavy Compatibility or Migration Plan.
- Generic Drawbacks or Risks.
- Weak Alternatives Considered.

**Optional** — nice-to-haves:

- Style, polish, formatting, and passive constructions.
- Suggestions for stronger Prior Art coverage.
- Cross-references to related LEPs or ADRs that could be linked.

For each item: section name, line number where possible, and a one-line description.

End the report with a one-line summary: "Required: N. Recommended: N. Optional: N." If Required is 0, state that the LEP passes structural and citation review.

### Step 7: Stop

Do not edit the LEP. Reviewing is read-only; the author makes the changes.

## Notes

- Be specific. "Section X has a problem" without a line number or quote is not actionable. Always cite the line and quote enough text for the author to find the issue.
- Be charitable in Phase 3. Push back on weak rationale, but recognize that judgment calls are not always black-and-white. If you're unsure whether something is a generic hedge or a real drawback, classify it as Optional with a note.
- Do not require citations for claims the LEP itself is the source for — the author proposing a new design does not need to cite themselves.
- Do not run the review on `docs/proposals/lep-template.md` or `docs/proposals/README.md`. If the user points at one of those, decline and explain why.
