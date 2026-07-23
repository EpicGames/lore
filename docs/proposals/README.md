# Lore Enhancement Proposals

This directory holds **Lore Enhancement Proposals (LEPs)** — public-facing,
pre-implementation proposals for changes to the Lore wire protocol, on-disk
format, public APIs (CLI, capi, JS bindings), and cross-cutting features. LEPs are where the design is debated; ADRs in
[`../developing/decisions/`](../developing/decisions/README.md) record narrow architecture
decisions after they are made.

## Lifecycle

LEPs use a three-state lifecycle:

- **Draft** — being authored or under review.
- **Accepted** — approved; implementation may proceed.
- **Rejected** — evaluated and not pursued. Rejected LEPs stay in-tree as historical record.

Filenames follow the `YYYY-MM-DD-kebab-name.md` convention.

## How to write an LEP

Copy [`lep-template.md`](lep-template.md) to `YYYY-MM-DD-<your-slug>.md` in this directory and fill each section, using the rules below as a guide. Open a CR when you're ready — reviewers will look for the same things.

### Throughout the proposal

These apply across every section.

- **Active voice.** Write "this proposal rejects approach X", not "approach X was rejected"; "Lore writes the new version", not "the new version is written by Lore". Past-participle adjectives ("the upgraded client", "the proposed design") are fine.
- **Public citations.** LEPs are public. Support every factual claim either with a public link (RFC, code path in the Lore tree, a public ADR, an upstream project doc) or by capturing the relevant detail inline. Internal-only resources (CRs, JIRA tickets, Slack threads, internal wikis) belong in the `discussion` frontmatter field — they don't substitute for support in the body.
- **Ground every claim.** If you can't back something up with research, prior discussion, or the proposal itself, leave it out rather than filling space.

### Summary

- One paragraph.
- Written for a reader unfamiliar with the area.

### Motivation

- Describe the problem this solves and why it matters now: the incident, limitation, user need, or external pressure that prompted the proposal.
- Stay on the problem. Save the solution for Proposed Design.

### Goals / Non-Goals

- Goals are the outcomes you want this proposal to achieve. Each one should be addressed in Proposed Design.
- Non-Goals are adjacent problems explicitly out of scope. They're useful when scope creep comes up during review.
- Skip toothless Non-Goals — items so obvious nobody would expect them.

### Proposed Design

- Aim for the level of detail that makes the case for your Goals. Implementation detail belongs in a downstream spec.
- For each Goal, point to the part of the design that addresses it. If a Goal has no matching design, either move it to Non-Goals or extend the design.
- Every problem you raised in Motivation should be addressed somewhere here. Design content that traces back to neither a Motivation problem nor a Goal is probably over-scoped.

### Compatibility

If your proposal touches an external surface, answer the questions below. Use `N/A` when a sub-section doesn't apply (a short reason after the dash helps reviewers but isn't required). Add custom sub-sections for any other compatibility surface your proposal touches (configuration formats, environment variables, third-party library APIs, plugin interfaces).

- **Wire format** — Can a v(N-1) peer decode messages produced by v(N)? Can v(N) decode v(N-1)? What changes in serialization, framing, compression, or byte layout?
- **Client/server protocols** — What new or changed RPCs, request/response shapes, message types, or authentication flows does this introduce? What does an old client see when it talks to a new server, and vice versa?
- **On-disk format** — Can an upgraded Lore read existing repositories? Can a downgraded Lore read data written by the new version? What fragment-flag, index, or schema changes apply?
- **CLI and public API** — What changes in `lore` subcommand syntax, exit codes, output format, or the surface of `lore-capi` / JS bindings? What breaks for existing scripts and integrations?

When a sub-section isn't `N/A`, be concrete — specific bits, specific commands, specific protocol fields. Vague answers like "there will be some impact" don't give reviewers much to work with.

### Non-Functional Considerations

For each property below, answer the question. If your proposal preserves the property, say so in a line. If it changes the property, name the impact and how the design handles it. Add custom sub-sections for any other non-functional concern (latency, throughput, durability, ordering guarantees, backpressure, error budgets).

- **Concurrency** — How does the proposal behave under concurrent reads, writes, commits, syncs, and merges?
- **Memory** — Does the proposal require buffering data proportional to repository or file size, or does it preserve Lore's streaming and sparse-data-structure model?
- **Statelessness** — Does the proposal introduce process- or library-level state that survives across operations?
- **Determinism** — Do the same inputs continue to produce the same outputs, including history?

### Migration Plan

- Needed when any Compatibility sub-section flags a breaking change.
- Name the transition phases (e.g. dark launch, early transition, late transition).
- For each phase, describe the read/write compatibility matrix and the tooling, flags, telemetry, and operational guidance users will need.
- Cover rollback: what observable signal triggers it, and what state is recoverable.
- If no breaking changes are involved, write `N/A — no breaking changes, no migration required.`

### Security Considerations

- Whether this changes the trust model.
- Whether a malicious peer or crafted repository can abuse the new behavior.
- Whether any new data is integrity- or confidentiality-sensitive.
- If your proposal has no security implications, say so and explain why — bare `N/A` isn't enough.

### Privacy Considerations

- What new user data, identifiers, file paths, or metadata become visible to other parties — server operators, peers, telemetry pipelines, logs.
- Whether the change affects the ability to delete, redact, or expire data.
- If your proposal has no privacy implications, say so and explain why — bare `N/A` isn't enough.

### Risks and Assumptions

Use two sub-headed lists.

- **Assumptions** — premises that, if wrong, would invalidate the proposal. Each carries an `*invalidated if:*` clause.
- **Risks** — things that could go wrong even if the assumptions hold. Each carries a `*mitigation:*` clause (or explicit acceptance).
- Keep these specific to your proposal. Generic risks like "this could have unexpected interactions" or "performance might be worse" don't add much.

### Drawbacks

- Real costs of this approach.
- Not premises that could be wrong (those are Assumptions). Not failure modes (those are Risks).
- One sentence each.
- Skip generic hedges like "more code to maintain" or "may require documentation updates".

### Alternatives Considered

- At least two alternatives, each with a concrete reason for rejection.
- Include the status quo if it's relevant.
- "This would be harder" isn't a reason — be specific about what makes the chosen approach better.

### Prior Art

- How other systems (Git, Mercurial, Perforce, Jujutsu, Pijul, Sapling, etc.) handle this problem, what's worth borrowing, and what's worth avoiding.
- Optional — leave the section out if no precedent applies.

### Unresolved Questions

- Open questions to resolve during review.
- Empty (or down to follow-up tickets) by the time the LEP is `Accepted`.
