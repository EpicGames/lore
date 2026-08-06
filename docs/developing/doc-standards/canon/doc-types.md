# Doc types

> Lore documentation has two type families. **Product docs** describe Lore as a product — how to use it, how it works, what its surfaces are. They live in `docs/` and use the four [Diátaxis](https://diataxis.fr/) types: Tutorial, How-To, Reference, Explanation. **Contributing docs** describe how to work on the Lore project — environment setup, coding conventions, architecture decisions. They live in `docs/developing/` and use the contributor types plus the How-To type. This file also documents Landing pages, which are structural folder-index pages rather than a content type. Page structure (headings, lists, tables, formatting) lives in [`format.md`](format.md). Voice, mood, word choice, and links live in [`language.md`](language.md). Filename conventions live in [`../operational/filenames.md`](../operational/filenames.md).

Each Lore page is one of seven content types — four product-docs (Tutorial, How-To, Reference, Explanation) and three contributing-docs (Internals, ADR, Code-Standard); How-To is valid in both trees — plus the structural Landing page used for folder indexes. This file defines each, says when to use it (and when not to), specifies the required structure, and provides a copy-paste Markdown template.

## Two type families

Lore docs have two type families, distinguished by topic:

- **Product docs** at `docs/tutorials/`, `docs/how-to/`, `docs/reference/`, `docs/explanation/`. Cover Lore as experienced by developers, integrators, and operators. Contributors read these too — an API reference belongs here even though contributors use it, because it documents the product.
- **Contributing docs** at `docs/developing/`. Cover the Lore project as a contribution target: internals, decisions, coding conventions, and contributor setup. The How-To type is also valid here for contributor-facing procedural guides (for example, setting up a build environment).

Route by topic, not by reader. A reference page for the Rust API belongs in `docs/reference/` even though contributors read it — it documents the product. A guide to setting up a build environment belongs in `docs/developing/` even though users could read it — it documents contributing to the project. When a topic spans both families with enough depth to warrant different coverage depths, split into two linked pages; otherwise one product-docs page that contributors also read is fine. The same caution applies within the product-docs tree when CLI-user and operator perspectives diverge: write two pages that link to each other.

## Choose the right doc type

Match the question the reader is asking to the doc type. Picking the wrong type forces structure to fight the content.

### User-facing types

| Reader intent | Doc type | Folder | Mood | Length |
| --- | --- | --- | --- | --- |
| "Walk me through learning X." | **Tutorial** | `tutorials/` | Imperative for steps | 300–2000 |
| "I have a goal — give me the recipe." | **How-To** | `how-to/` | Imperative for steps | 200–800 typical |
| "What does each option/field/flag do?" | **Reference** | `reference/` | Indicative | As long as the surface |
| "Help me understand X / why X is so." | **Explanation** | `explanation/` | Indicative | 300–3000 |

Decision rules of thumb:

- The reader is **learning by doing**, with an introductory mindset: Tutorial.
- The reader **knows what they want done** and needs the recipe: How-To.
- The reader needs to **look up a fact**: Reference.
- The reader needs to **understand**: Explanation.

### Contributor types

| Contributor question | Doc type | Folder |
| --- | --- | --- |
| "How's this part of Lore actually built?" — byte layouts, on-disk formats, wire protocols, internal struct shapes. | **Internals** | `docs/developing/internals/` |
| "Why did the project choose X over Y?" — durable record of an architectural decision and its trade-offs. | **ADR** | `docs/developing/decisions/` |
| "How do I write source code that conforms to Lore conventions?" — language- or area-scoped rules with rationale and examples. | **Code-Standard** | `docs/developing/code-standards/` |

A single page is one type. If you find yourself writing a Tutorial that turns into a Reference, split into two linked pages. If you find yourself writing an Internals page that pivots into a decision rationale, split into an Internals page plus an ADR.

### Worked example: "I added a new feature, what do I write?"

A new feature doesn't usually imply one doc. Walk this list:

- **New CLI command, flag, config key, or API surface?** Write a **Reference** page (or update an existing one) cataloging the surface. Almost always required.
- **New model, concept, or behavior the reader needs to understand?** Write an **Explanation** page describing the model. Required when the reader can't use the feature productively without understanding it.
- **A discrete goal the feature unlocks ("set up X," "recover from Y")?** Write a **How-To** page. Optional. Add one when there is a real goal-oriented procedure that crosses two or more commands.
- **Architectural decision worth recording for future contributors?** Write an **ADR** in `docs/developing/decisions/`. Optional. Add one when the decision involved a real trade-off and discarded alternatives need to outlive the discussion.
- **Implementation detail (byte layouts, internal protocols, struct shapes) other contributors will need?** Write an **Internals** page. Optional. Add one when contributors have to reason about the format from another tool or module.
- **New coding rule the feature establishes for future code?** Write a **Code-Standard** page. Rare from a single feature, but possible.

Tutorials are the one type a single feature usually doesn't trigger. Tutorials get authored as a deliberate teaching artifact, often spanning multiple features. If you're writing one, treat the feature as raw material rather than the structure.

### The Diátaxis compass

User-facing types arrange on two axes:

- **Action vs Cognition.** Tutorials and How-Tos are about *doing*. References and Explanations are about *knowing*.
- **Acquisition vs Application.** Tutorials and Explanations serve the reader's *study* (acquiring something new). How-Tos and References serve the reader's *work* (applying what they already know).

| | **Acquisition (study)** | **Application (work)** |
| --- | --- | --- |
| **Action (doing)** | Tutorial | How-To |
| **Cognition (knowing)** | Explanation | Reference |

When you can't decide between two types, use the compass. A doc that mixes acquisition and application — for example, a "tutorial" that pivots into a flag table halfway through — is two docs jammed together. Split them.

## Tutorial

### Definition

A Tutorial is procedural, task-based documentation that walks the reader through learning a new skill by doing. The reader is a beginner; the instructor is the page. The page is responsible for the reader's success and safety along the way. It has a defined start, end, and success condition.

### When to use it

- The reader is new to the topic and needs to build a working mental model by doing.
- The task has a defined start, end, and success condition.
- The task takes more than two or three steps.

### When not to use it

- The reader already knows the basics and needs the recipe. That's a How-To.
- The task is a single command. Document it in the Reference page.
- The "tutorial" is a tour of a feature with no concrete outcome. Make it an Explanation.
- The reader needs to make their own decisions at each step. Pair a system-style Explanation with a Reference instead.

### Required structure

1. H1 page title.
2. One-paragraph lede: what the reader will accomplish and why.
3. **Prerequisites** — a bulleted list of what the reader needs before starting (installed software, permissions, prior knowledge, prior tutorials).
4. **Steps** — a numbered list of imperative-mood actions. Each step is one action.
5. **Verify** or **Result** — what the reader should see when the tutorial is complete; how to confirm success.
6. **Troubleshooting** (optional) — common problems and fixes.
7. **Next steps** — what to read or do next.

### Voice and length

- Imperative mood for steps: "Run," "Open," "Set." Not "You should run."
- Indicative mood for the lede, prerequisites, and verification.
- Use **you** for the reader. Never use **we** for the instructional voice ("In this tutorial, we will create a branch") — rewrite to imperative or second person. Project-voice **we** (decisions, recommendations, what the project provides) is valid in Tutorial context paragraphs. See [`language.md` § Pronouns](language.md#pronouns).
- Length: 300–2000 words for a single page; longer tutorials split into multi-page tutorials (parent + child pages).
- Three to ten steps per page. More than ten: split into sub-steps under numbered parents, or break into multiple linked pages.

### Template — single-page tutorial

See [`docs/tutorials/tutorial-template.md`](../../../tutorials/tutorial-template.md) — copy this file when starting a new Tutorial.

### Template — multi-page tutorial parent

A multi-page Tutorial uses a parent page that introduces the tutorial and lists ordered child pages. Each child page is itself a single-page Tutorial that picks up where the previous left off.

Adapt [`docs/tutorials/tutorial-template.md`](../../../tutorials/tutorial-template.md) for the parent page: keep the same heading structure, replace **Steps** with a numbered list of links to ordered child pages under the heading **Tutorial steps**, add a **What you'll build** section between **Prerequisites** and **Tutorial steps**, and rename **Next steps** to **What's next**.

Each child page filename includes the parent's slug as a prefix (`<parent>-part-N-<title>.md`) so the relationship is visible in the path and in the navigation. At the bottom of each child page, add a **Continue** section with `Previous:` and `Next:` link bullets pointing at the sibling child pages.

## How-To

### Definition

A How-To is a recipe that addresses a specific goal a competent reader has *right now*. The reader already knows the basics; the page exists to give them the shortest path to a result they can describe up front. How-Tos pair with Tutorials and References: Tutorials teach the underlying skill, References catalog the surface, How-Tos package one applied procedure.

### When to use it

- The reader knows what they want to accomplish (*"set up zsh completions,"* *"configure a shared store,"* *"recover from a failed merge"*) but not the exact incantations.
- The task crosses two or more commands or files and benefits from being captured as one procedure.
- The reader will skim, not read top-to-bottom.

### When not to use it

- The reader is a beginner who needs to build the underlying mental model. Write a Tutorial.
- The task is a single command. Document it as an example in the Reference.
- The page would be mostly *why* the configuration is set this way. Write an Explanation.
- The procedure is the new-user golden path from zero to working. That's a Tutorial.

### Required structure

1. H1 page title — verb-first goal phrasing ("Set up zsh completions", "Recover from a failed merge").
2. One-paragraph context: when you would reach for this guide and the assumed starting state.
3. **Before you start** (optional, terse) — assumed prerequisites in one or two bullets, *not* the full Tutorial-style prerequisites list.
4. **Steps** — numbered, imperative, terse. No pedagogy. Inline commands and minimal explanation.
5. **Result** (optional, one line) — what the reader sees when it worked.
6. **See also** — links to the Reference for the surfaces touched and the Explanation for the why.

### Voice and length

- Imperative mood for steps. Indicative mood for context.
- Active voice. Second person ("you").
- 200–800 words typical. Past 1000 words, ask whether it's becoming a Tutorial — a How-To with a tricky one-time setup can legitimately run 1100 words, but if pedagogy is creeping in (background, mental model, "what we just did"), split it.
- No "Verify" section, no Troubleshooting subtree, no "Next steps" pointer to follow-up tutorials. Those belong in Tutorials.

### Template

See [`docs/how-to/how-to-template.md`](../../../how-to/how-to-template.md) — copy this file when starting a new How-To.

## Reference

### Definition

A Reference page is the in-depth, exhaustive description of a Lore surface — a command, a config file, an API. It catalogs every option, flag, field, or input. The reader is at *work* and looking up a *fact*, not learning. They will arrive via search or a deep link, not by reading the page top-to-bottom.

### When to use it

- The page documents a CLI command and all its flags.
- The page documents a config file and all its keys.
- The page documents an API endpoint or library function.
- The reader will arrive via search or via a direct link from another page.

### When not to use it

- The page would teach the reader why or when to use the surface. Pair it with an Explanation.
- The page would walk the reader through using the surface. That's a Tutorial.
- The surface has fewer than three options/fields. Inline it in the relevant Explanation or Tutorial.

### Required structure

1. H1 page title (the command, file, or function name in canonical form).
2. **Synopsis** — the signature: command + flags, function signature, file schema header.
3. One-paragraph lede: what the surface is and what it does.
4. **Options / Flags / Fields** — a table or section per option, in a consistent order (alphabetical or by frequency).
5. **Examples** — three to five worked examples covering common use cases.
6. **Exit codes** (optional, where applicable) — table of exit codes and their meanings, for command-line surfaces.
7. **See also** — links to related Reference pages, the Explanation that explains the model, and the Tutorial that uses it.

### Voice and length

- Indicative mood for descriptions; imperative mood inside example commands and "Use this when X" notes.
- Active voice, present tense, third person ("This flag enables X") or second person ("Use this flag when you X").
- Tables for option-and-description pairs. See `format.md` for table formatting rules.
- Length: as long as the surface requires. Reference pages can run several thousand words; that's the cost of being exhaustive.
- Keep cell content short. Long descriptions go below the table in a per-option subsection.

### Template

See [`docs/reference/reference-template.md`](../../../reference/reference-template.md) — copy this file when starting a new Reference.

## Explanation

### Definition

An Explanation page makes a topic understandable. It supplies context, background, and reasoning — *why* the system works the way it does, *what* the underlying model does, *how* the topic connects to its neighbors. Explanation belongs to acquisition + cognition on the Diátaxis compass: the reader is studying, not working.

Explanation is content-shaped. A page about "Why Lore?" reads differently from a page describing the commit object model, and both read differently from a one-page primer that sits in front of a Tutorial. The required structure below is loose by design; the worked examples that follow show the three shapes you'll most often produce in Lore docs.

### When to use it

- The reader needs to understand a Lore subsystem before they can use it productively.
- A design decision needs context — *why* the system works this way — that doesn't belong inside step-by-step instructions.
- A topic crosses several Reference pages and benefits from a single explanatory home.
- The page is the written companion to a talk, whitepaper, or rationale that needs a permanent home in the docs.

### When not to use it

- The page would be mostly step-by-step instructions. That's a Tutorial or a How-To.
- The page would be mostly tables of options or fields. That's a Reference.
- The page would route readers without explaining anything. That's a Landing.

### Required structure

1. H1 page title.
2. One-paragraph lede: what the reader will understand after reading the page.
3. Body sections — flexible H2 sequence depending on the explanation's shape (see worked examples).
4. **Related** or **See also** — short links to neighboring Explanations, References, and Tutorials.

Section *content* is type-driven; section *order* is shape-driven. Pages within one shape stay parallel — see worked examples below.

### Voice and length

- Indicative mood throughout. Explanations describe and reason; they don't instruct.
- Active voice. Past tense for history; present for current behavior; future tense only for items explicitly tagged as roadmap.
- Use **you** for the reader. Use **we** for Lore-project recommendations (per `language.md`). Never **I**.
- Length: 300–3000 words. The system / model variant trends 500–1500; the long-form rationale variant trends 1000–3000; the primer variant: 200–800 words.

### Template

See [`docs/explanation/explanation-template.md`](../../../explanation/explanation-template.md) — copy this file when starting a new Explanation.

### Worked example: Why this exists (long-form rationale)

A long-form rationale doc — like *Why Lore?* or *Why we chose CRDT-based storage* — explains the reasoning behind a Lore design decision and what it means for adopters. It's the written companion to a talk, a whitepaper, or a charter section.

Typical H2 sequence:

1. **The problem.** What real-world need motivates this. Concrete, not abstract.
2. **Existing approaches and where they fall short.** A short, fair tour. Cite specifics. Don't strawman.
3. **What's different about Lore's approach.** The decision and what it costs and gains.
4. **Trade-offs and posture.** What this implies for adopters today, especially in the 0.x window.
5. **Implications.** What changes for the reader's plans, given the above.

Length: 1000–3000 words. This shape rewards depth; don't pad. Cite source material where possible. Avoid corporate or marketing voice — Lore docs use the project's collective maintainer voice.

### Worked example: System / model explanation

A system / model Explanation describes a Lore subsystem — commits, branches, fragments, the object store — at the level of model, not API surface. It pairs with a Reference page that catalogs the surface, and with a Tutorial that walks through using it.

Typical H2 sequence:

1. **Why it exists** or **Why this matters** — the problem the subsystem solves.
2. **How it works** — the model. Diagrams or worked examples as needed. H3 subsections for distinct parts of the model.
3. **Worked example** (optional) — a single concrete walkthrough showing the model in action.
4. **Related** — short links to the Reference catalog, the Tutorial that exercises the subsystem, and adjacent Explanations.

Length: 500–1500 words. If the page would push past 1500 words, split into multiple Explanation pages, each handling one part of the model.

### Worked example: Short explanation primer

A primer is a 200–800 word Explanation that sits in front of a deeper Tutorial or Reference and gives the reader the high-level shape of a topic before they invest. *Why Lore?* is one example. A primer that introduces a subsystem before its full model Explanation is another.

Typical H2 sequence:

1. **The thing itself** — two or three short paragraphs.
2. **Why it matters** — the value or constraint this serves.
3. **Where to go next** — links to the deeper Tutorial / Explanation / Reference pages on the topic.

Length: 200–800 words. Past 800, the page is becoming a system / model Explanation; promote it.

## Internals

### Definition

An Internals doc is the austere, descriptive reference for a piece of Lore's implementation. It catalogs what the machinery looks like under the hood — byte layouts, struct shapes, serialization formats, wire protocols, internal state machines. The reader is at the source code, trying to understand or modify it. The page is responsible for being **precisely correct**, not for teaching.

### When to use it

- The page documents an on-disk or on-wire format and a contributor needs to read or write that format from another tool.
- The page documents the shape of an internal struct or enum that contributors must reason about across module boundaries.
- The page documents an internal protocol — message ordering, state transitions, retry semantics — that isn't exposed to Lore users.

### When not to use it

- The page would explain *why* the implementation is the way it is. That's an ADR (if it captures a decision) or an Explanation in `docs/explanation/` (if the context is user-facing).
- The page would walk a contributor through making a change. That's a How-To in `docs/how-to/` (if about using the product) or a How-To in `docs/developing/` (if about contributing to the project).
- The page documents a public surface — CLI flags, library API, on-disk format that Lore users construct directly. That's a Reference in `docs/reference/`.

### Required structure

1. H1 page title (the format, struct, or protocol name).
2. One-paragraph lede: what this internal surface is and where it appears in the source.
3. Body sections per concept — one H2 per layout / struct / state machine. Use tables for byte-by-byte layouts and field-by-field struct definitions.
4. **Source pointers** — list of source paths and symbols that implement or consume this surface.
5. **See also** — links to related Internals docs and any ADRs that constrain this surface.

### Voice and length

- Indicative mood throughout. Descriptive, not instructional. No "you should …," no "we recommend …."
- Active voice, present tense. Write what the format **is**, not what it **does** for the reader.
- Tables for any fixed-shape structure — byte layouts, field tables, message-type enumerations.
- Length: as long as the surface requires. Internals pages stay focused on one surface; if a page sprawls past several thousand words, split by sub-surface.

### Template

````markdown
# <Internal surface name>

<One paragraph: what this surface is, where it appears in the source, and
which other Lore subsystems consume it.>

## <First H2 — one concept>

### Layout

| Offset | Size | Field | Description |
| -------- | ------ | ------- | ------------- |
| `0x00` | 4 B  | `magic` | Constant `0xLORE` (network byte order). |
| `0x04` | 4 B  | `version` | Schema version. Increment on incompatible changes. |
| `0x08` | n B  | `payload` | Length-prefixed payload. |

### Encoding rules

<Constraints, byte order, alignment, padding rules.>

## <Second H2 — another concept>

<Body.>

## Source pointers

- `lore-core/src/<file>.rs::<symbol>` — the canonical implementation.
- `lore-server/src/<file>.rs::<symbol>` — the consumer that decodes this format.

## See also

- [<Related internals page>](<page>.md)
- [<decision that constrains this surface>](../decisions/NNNNN-<slug>.md)
````

## ADR

### Definition

An ADR — Architectural Decision Record — is the durable record of a single architectural decision the Lore project has made. It captures the **context** the decision was made in, the **decision** itself, and the **consequences** the project lives with as a result. ADRs are append-only history: once accepted, they aren't edited substantively. A change of mind produces a new ADR that supersedes the old one and links back.

### When to use it

- A decision shapes the project's architecture in a way that future contributors will need to understand to avoid relitigating.
- A decision involves a trade-off where the discarded alternatives were plausible, and the rationale needs to outlive the discussion that produced it.
- A decision constrains how downstream code or other decisions are made.

### When not to use it

- The "decision" is a routine implementation choice with no real trade-off. Document it in code or an Internals doc, not an ADR.
- The decision is provisional and the team expects to revisit it within weeks. Use an RFC or a tracked issue until it stabilizes.
- The page would primarily explain a subsystem rather than record a choice. Use an Internals doc or a user-facing Explanation.

### Header block

ADRs begin with a YAML header block. Required fields:

- `status` — one of `proposed`, `rejected`, `accepted`, `deprecated`, `superseded by ADR-NNNNN`. Use lowercase only. Status is the only field that changes after acceptance.
- `date` — `YYYY-MM-DD` of the last status update.

Optional fields (omit when not relevant):

- `deciders` — everyone involved in the decision.
- `consulted` — subject-matter experts whose opinions were sought (two-way communication).
- `informed` — people kept up-to-date on progress (one-way communication).

### Required structure

1. H1 page title — a short statement of the problem and the chosen solution, prefixed with `ADR-NNNNN:` to match the sequence number in the filename (for example, `# ADR-00012: Log dispatch in core library`).
2. **Context and Problem Statement** — what circumstance, constraint, or problem motivated the decision. Free form, two to three sentences, or an illustrative story. Link out to collaboration boards or issue trackers when useful.
3. **Decision Drivers** — bulleted list of forces, constraints, or concerns that shape the decision.
4. **Considered Options** — bulleted list of the option titles weighed before the decision.
5. **Decision Outcome** — names the chosen option and gives the justification. Includes a nested `### Consequences` H3 listing the *Good, because …* and *Bad, because …* outcomes the project lives with.

Optional sections (use when the content warrants):

- **Pros and Cons of the Options** — between *Decision Outcome* and *More Information*. One H3 per option, with bulleted *Good, because …*, *Neutral, because …*, *Bad, because …* arguments.
- **More Information** — at the end. Additional evidence, team agreement, links to other decisions, or notes on when the decision should be revisited.

### Filename pattern

`docs/developing/decisions/NNNNN-<slug>.md`, where `NNNNN` is zero-padded and `<slug>` is the lowercased, hyphenated form of the decision title (drop articles).

Numbering is monotonic across the project. A new ADR claims the next available number; never reuse a number even for superseded ADRs. The H1 itself doesn't include the number — readers find it via the filename and the directory listing.

### Voice and length

- Past tense for **Context and Problem Statement**. Present tense for **Consequences** and any project rules the decision codifies.
- Active voice. Use **the project**, **Lore**, or **we** for the deciding body — match the project's collective maintainer voice (see `language.md`).
- Length: 200–1000 words typical. ADRs trend short and focused — one decision per ADR. If the document grows past 1000 words, the decision is probably two decisions and should be split.

### Immutability and supersession

Once an ADR moves from `proposed` to `accepted`, the body is **immutable** except for the `status` and `date` header fields. To change a decision, write a new ADR, set its `status` to `accepted` with today's `date`, link to the prior ADR from *More Information*, and edit the prior ADR's `status` to `superseded by ADR-NNNNN`. The prior body stays unchanged so the original record remains readable.

### Template

See [`docs/developing/decisions/adr-template.md`](../../decisions/adr-template.md) — copy this file when adding a new ADR.

## Code-Standard

### Definition

A Code-Standard doc is a language- or area-scoped set of conventions governing how Lore source is written. It pairs imperative rules with rationale, code examples, and reference tables — the log levels, error variants, macros to call, and which crate follows which pattern. The reader is a contributor about to write or review code in that area; the page exists to give them the conventions in a form they can apply directly.

### When to use it

- A language has its own idioms in Lore that differ from the language's defaults — `errors.md` for Rust error-handling patterns, for example.
- An area of the codebase has cross-file conventions — how tasks are spawned, how logs are emitted, how tests are structured.
- A reviewer needs a stable reference to cite when requesting changes.

### When not to use it

- The convention is a single line of text and exists in only one place. Inline it as a comment on the relevant module.
- The "rule" amounts to an architectural decision. Write an ADR and have the Code-Standard cite it.
- The convention is enforced automatically by lint or formatter and the human-readable form would duplicate the tool's docs. Cite the lint config and stop there.

### Required structure

A Code-Standard page opens with an H1 — the language and area — and a one-sentence lede naming what it covers. The body is a sequence of thematic rule sections; add the optional sections below when they fit the area.

1. H1 page title — the language and area (for example, "Lore error handling standards").
2. One-sentence lede — what the page covers.
3. **Overview** (optional) — the high-level model the rules sit on: the layers, systems, or test types involved, as a short list or table.
4. **Rule sections** — one H2 per concern, numbered (`## 1. Defining error types`) or named (`## Task cancellation`). Each section states its rules in the imperative, gives the rationale, and shows the pattern with a fenced code example. Use a good/bad pair where the contrast clarifies, and a table for any enumeration (log levels, error variants, methods, config fields).
5. **Crate-specific patterns** (optional) — where the rules vary by crate, a section grouping crates by the pattern they follow.
6. **Best practices** (optional) — a numbered recap of the load-bearing rules.
7. **Key files** or **See also** (optional) — the source files that define the surface, and links to related Code-Standards, Internals docs, or ADRs.

### Voice and length

- Imperative mood for rule statements; indicative mood for rationale and overview prose.
- Active voice. Use **we** for project recommendations, **you** when addressing the reader directly.
- Code examples reflect real Lore identifiers — crate names, macros, types — so a reader can map them straight onto the source. Keep each example minimal and focused on the rule it illustrates.
- Tables for enumerations: log levels, error variants, per-crate patterns, config fields.
- Length: as long as the area requires. A narrow area might run 200 words; a broad one several thousand. Quality is that every rule has a rationale and a concrete example, not word count.

### Template

````markdown
# Lore <area> standards

<One sentence: what this page covers.>

## Overview

<The high-level model the rules sit on — the layers, systems, or test
types involved. A short list or table.>

## 1. <First rule section>

<Imperative rule statement and the rationale behind it.>

```<lang>
<minimal example showing the conforming pattern>
```

## 2. <Second rule section>

<Rule and rationale. Show a good/bad pair where the contrast clarifies:>

```<lang>
// DON'T DO THIS
<non-conforming pattern>

// DO THIS
<conforming pattern>
```

## Crate-specific patterns

### <Pattern name>

- **<crate>**, **<crate>**

## Best practices

1. **<Imperative recap of a load-bearing rule>** — <short gloss>.

## Key files

| File | Purpose |
| --- | --- |
| `<crate>/src/<file>.rs` | <what it defines> |
````

## Landing pages (structural, not a content type)

> A Landing page is the `README.md` of a content folder. Authors don't pick between writing a Landing and writing a Tutorial. Update a Landing when a folder's index needs maintenance: a new doc lands in the folder, a child page is renamed or moved, or the section's purpose shifts.

### Definition

A Landing page orients readers to a folder — what's in it, who it's for, and where to start. It always has child pages and exists to route readers to the right deeper page. A Landing is short, scannable, and explicitly routes — it isn't where the reader learns the topic.

### When to use it

- A content folder has more than three child pages and readers need help finding the right one.
- An entire section of the site needs an entry point at the top of the navigation tree.
- A grouping of Reference, Tutorial, How-To, or Explanation pages benefits from a one-page summary with links.

### When not to use it

- There are fewer than three child pages — link to them from the parent topic instead.
- Readers will land here from search and read straight through. That's a short Explanation primer.
- The page would teach the topic itself. That's a Tutorial or Explanation.

### Required structure

1. H1 page title.
2. One-paragraph lede: what this folder covers and who it's for.
3. **Where to start** (optional) — a recommended first page for new readers.
4. **What you'll find here** or topical sections — short groupings of links to child pages, each with one sentence of context. Group by content type (Tutorials / How-To / Reference / Explanation) when the folder mixes types.
5. **Key concepts** (optional) — a bulleted list of foundational concepts in this area, each linked to its Explanation page. Useful when readers benefit from naming the concepts before diving into the docs.
6. **Where to go next** (optional) — links onward by reader intent ("New to the topic," "Ready to build something," "Looking for a specific command").
7. **Related areas** (optional) — links to adjacent Landing pages or top-level docs.

### Voice and length

- Indicative mood. Plain present tense.
- Length: 100–400 words. A Landing should fit on one screen on a typical laptop without scrolling much.
- Don't duplicate content from child pages. The job of the Landing is to route, not to teach.

### Template

```markdown
# <Topic area>

<One paragraph: what this section of the docs covers and who should be
reading it.>

## Where to start

If you are new to <topic area>, start with [<recommended first page>](<page>.md).

## Tutorials

- [<Tutorial 1>](<tutorial-1>.md) — <one-sentence description>
- [<Tutorial 2>](<tutorial-2>.md) — <one-sentence description>

## How-To guides

- [<How-To 1>](<how-to-1>.md) — <one-sentence description>
- [<How-To 2>](<how-to-2>.md) — <one-sentence description>

## Reference

- [<Reference 1>](<reference-1>.md) — <one-sentence description>

## Explanation

- [<Explanation 1>](<explanation-1>.md) — <one-sentence description>

## Key concepts

- [**<Topic 1>**](../explanation/<topic-1>.md): <one-sentence definition>
- [**<Topic 2>**](../explanation/<topic-2>.md): <one-sentence definition>

## Where to go next

- New to the topic: read [<Explanation 1>](../explanation/<explanation-1>.md).
- Ready to build something: try [<Tutorial 1>](../tutorials/<tutorial-1>.md).
- Looking for a specific command: see [<Reference 1>](../reference/<reference-1>.md).

## Related areas

- [<Adjacent Landing>](<adjacent-landing>.md)
```

## Glossary

> Glossary isn't a content type. The Lore project maintains a single glossary file at [`docs/glossary.md`](../../../glossary.md). Updates are routine — when a Lore-specific term enters the documentation, add an entry.

The entry-writing rules — alphabetical order, headword formatting, sense disambiguation, cross-references, part-of-speech tags — live in [`../operational/glossary-conventions.md`](../operational/glossary-conventions.md). Load that file when adding to or maintaining the glossary.

## Cross-type notes

### Pages within one type must use parallel structure

- All Tutorials use the same H2 sequence: lede, Prerequisites, Steps, Verify, Troubleshooting (optional), Next steps. Don't invent new section names.
- All How-Tos use the same H2 sequence: lede, Before you start (optional), Steps, Result (optional), See also.
- All References use the same H2 sequence: Synopsis, lede, Options, Examples, Exit codes (where applicable), See also.
- All Internals docs use the same H2 sequence: lede, body sections per concept, Source pointers, See also.
- All ADRs use the same H2 sequence: Context and Problem Statement, Decision Drivers, Considered Options, Decision Outcome (with nested Consequences), Pros and Cons of the Options (optional), More Information (optional). `status` and `date` are header fields declared above the H1, not H2s.
- All Code-Standards open with a lede and a sequence of thematic rule sections (numbered or named), adding Overview, Crate-specific patterns, Best practices, and Key files or See also sections as the area warrants.
- All Landings use the same H2 sequence as appropriate to the folder: lede, Where to start (optional), per-type sections, Key concepts (optional), Where to go next (optional), Related areas (optional).
- Explanations are content-shaped; pages within the same shape (long-form rationale, system / model, primer) stay parallel within that shape.
- Why: parallel structure across pages of the same type means readers know exactly where to look without re-orienting per page.

### Mood is type-driven

- Tutorials and How-Tos use **imperative mood** for steps.
- References, Explanations, Internals, and Landings use **indicative mood** for descriptions.
- ADRs: past tense for Context and Problem Statement, present for Consequences.
- Code-Standards: imperative for rule statements, indicative for rationale.

### Length is type-driven

- How-Tos: 200–800 words typical; past 1000, ask whether it's becoming a Tutorial.
- Tutorials: 300–2000 words single-page; multi-page when longer.
- References: as long as the surface; tables-heavy.
- Explanations: 300–3000 words. Primer variant: 200–800 words; system / model variant trends 500–1500; long-form rationale variant trends 1000–3000.
- Landings: 100–400 words.
- Internals: as long as the surface.
- ADRs: 200–1000 words typical.
- Code-Standards: as long as the area requires.

### Audience is folder-derived

**Product docs** (under `docs/tutorials/`, `docs/how-to/`, `docs/reference/`, `docs/explanation/`) describe Lore as a product — its surfaces, behaviour, and concepts. **Contributing docs** (under `docs/developing/`) describe the Lore project as a contribution target — its internals, decisions, conventions, and contributor setup. The How-To type may appear in either tree: use `docs/how-to/` when the goal is about using the product, use `docs/developing/` when the goal is about contributing to the project. The folder location encodes the distinction — route by topic, not by reader. There is no explicit audience field in published doc frontmatter.

### Public-facing, but contributor-baseline

Contributor docs are still public; they ship in the open-source repository and are readable by anyone. The open-source posture rules apply (see the *Open-source posture* section of [`../operational/review-checklist.md`](../operational/review-checklist.md)): a Code-Standard must not reference internal sponsor-organization tooling, internal hostnames, internal ticket-tracker IDs, or private dependencies.

### Page templates are starting points, not contracts

Required sections must appear. Optional sections appear when the content warrants them. Section order is fixed; sub-section order is flexible. Word-count ranges in each "Voice and length" section are guidance, not hard limits.

## Notes

The multi-page Tutorial child-page filename pattern (`<tutorial>-part-N-<title>.md`) is chosen for clarity in URLs and navigation.
