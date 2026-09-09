# Glossary conventions

The rules for maintaining the Lore glossary at [`docs/glossary.md`](../../../glossary.md). Read this before you add or revise a glossary entry.

Voice and word-choice rules live in [`../canon/language.md`](../canon/language.md). For the glossary's role as a structural element, see [`../canon/doc-types.md` § Glossary](../canon/doc-types.md#glossary).

## When a term belongs in the glossary

Add a term to the glossary when at least one of the following is true:

- It's **specialized** to Lore or to version control, and a generalist reader won't know it. *(Examples: `revision`, `working tree`.)*
- It has **multiple senses** that need disambiguation. *(Example: `stage` (verb, the act) vs `stage` (noun, the object).)*
- It's an **acronym or abbreviation** whose expansion is load-bearing in Lore docs. *(Examples: `CR`, `URL`.)*
- It's a **product, feature, or tool name** with fixed capitalization the project has decided on. *(Examples: `Lore`.)*

The Lore glossary is for Lore-specific or version-control-specific terms. General English spelling preferences (`canceled` vs `cancelled`, `acknowledgment` vs `acknowledgement`) are handled by Vale's `Lore.AmericanSpelling` rule, not by glossary entries. General hyphenation, capitalization, and unit-spacing rules live in [`../canon/format.md`](../canon/format.md).

Project-internal-only terms (build infrastructure, internal team names) belong in a separate internal reference, not the public glossary.

## Entry format

### Section grouping and headword

Group entries under H2 alphabetical sections (`## A`, `## B`, …) so the file passes markdownlint MD001 (heading-level-increment). Only include letters that have at least one entry.

Use a Markdown H3 for the headword. The H3 produces a stable anchor that doc pages link to (`glossary.md#term-name`).

Write the headword exactly as it should appear in prose: correct capitalization, hyphenation, accents, and punctuation.

- `### Lore` for the product (capital L).
- `### lore` for the binary (lowercase, no code-span in the H3 itself; readers will see the surrounding context).
When the form depends on usage, add a part-of-speech tag in parentheses and list each form as its own entry on consecutive lines:

```markdown
### Stage (n)
The set of file paths marked...

### Stage (v)
To record the intent to include...
```

Within each letter section, sort entries alphabetically, case-insensitive. Acronyms sort by their letters, not their expansion. Part-of-speech tags don't affect sort order.

### Definition

Lead with what the term **is**. No preamble. No "this term refers to."

```markdown
<!-- correct -->
### Revision
A frozen snapshot of the entire repository tree. Each revision has a hash signature and a parent revision.
```

```markdown
<!-- incorrect -->
### Revision
This term refers to a snapshot that Lore saves in the repository.
```

Use plain present tense, third person. Don't address the reader as `you` inside a glossary entry.

```markdown
<!-- correct -->
A revision is an entry in a branch's history.
```

```markdown
<!-- incorrect -->
You can think of a revision as a snapshot you create whenever you commit.
```

Keep entries to one or two short sentences. The bar is "what does this term mean?" — not "everything about the concept." Feature-level entries can run a short paragraph; if an entry is growing beyond that, the topic probably belongs in an Explanation page that the entry then points at.

Spell out an acronym in full on first mention inside the definition, even when the term itself is the acronym:

```markdown
### CR
A change request (CR) is a proposed set of revisions submitted for review before merging.
```

### Sense disambiguation — separate entries

If a term has two senses (verb vs noun, lowercase generic vs uppercase named feature), give each sense its own entry on consecutive lines.

```markdown
### Stage (v)
To record the intent to include a file's change in the next revision (`lore stage <path>`).

### Stage (n)
The set of file paths marked for inclusion in the next revision.
```

### Cite authority for contested rules

When a spelling or capitalization rule has a non-obvious source, cite it in the definition.

```markdown
### NVIDIA
Per the NVIDIA brand site, the company name is always written all caps.
```

## Cross-references between entries

Use **See** when the entry's only content is a redirect to another entry:

```markdown
### Instance
See repository instance.
```

Use **Also see** when an entry references another in addition to its own definition:

```markdown
### Branch
A first-class movable pointer to a sequence of revisions.

Also see: latest, working tree.
```

Use **Compare to** when two entries are distinguishable and the reader benefits from contrasting them:

```markdown
### Latest
The pointer to the most recent revision on a branch, held in the mutable store.

Compare to: revision.
```

Write cross-reference targets in plain text. The doc-site renderer resolves them — don't hand-author hyperlinks in the glossary source.

## External links inside definitions

When a term has an external authoritative reference — a vendor's docs, a standard, an academic source — append a `For more information, see:` block at the end of the definition. Wrap each URL in angle brackets (`<https://…>`). That's a Markdown autolink, not `[text](url)` link syntax. The angle-bracket form renders as a clickable link in the built site, and it still reads as a plain URL in raw Markdown. Don't drop the brackets: the MkDocs configuration doesn't autolink bare URLs, so a plain URL renders as dead text and trips markdownlint's `MD034` rule.

```markdown
### Three-way merge
A merge algorithm that uses a common ancestor revision plus the two heads being merged. The result preserves changes from both sides where they don't conflict.

For more information, see:
Three-way merge <https://example.com/three-way-merge>
```

## Linking from doc pages back to the glossary

Link the first occurrence of a glossary term on a doc page to its entry. Leave later occurrences on the same page unlinked.

```markdown
<!-- correct -->
A [revision](../glossary.md#revision) is an entry in a branch's history. Every revision has a unique ID, and any revision can be checked out.
```

```markdown
<!-- incorrect — repeated linking is visual noise -->
A [revision](../glossary.md#revision) is an entry in a branch's history. Every [revision](../glossary.md#revision) has a unique ID, and any [revision](../glossary.md#revision) can be checked out.
```

Repeated linking makes the page harder to scan. Once is enough to give the curious reader an entry point.

Link only to the public glossary. Don't link to internal-only references.

## Quick authoring checklist

When adding or revising a glossary entry, work through this list:

- [ ] The term is specialized to Lore or to version control (or has another reason from [When a term belongs in the glossary](#when-a-term-belongs-in-the-glossary)).
- [ ] The headword is an H3 with the term written exactly as it should appear in prose (case, punctuation, diacritics).
- [ ] A part-of-speech tag is added when form depends on usage. Separate forms are separate entries on consecutive lines.
- [ ] The definition opens with what the term **is**, in plain present tense, third person. One or two sentences.
- [ ] Any acronym is expanded on first mention inside its own entry.
- [ ] Authority is cited when the rule is non-obvious (Merriam-Webster, brand site, project decision).
- [ ] `See`, `Also see`, or `Compare to` is added for related entries — plain text targets, no hand-authored links.
- [ ] The first mention of the term in any doc page links to its glossary entry; later mentions don't.
