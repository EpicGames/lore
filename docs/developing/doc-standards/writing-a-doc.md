# Writing a Lore doc

This page gets a community contributor from "I want to add a doc" to "I have something maintainers can review" in about ten minutes. It covers the rules that genuinely bind a small contribution. Voice consistency, accessibility, doc-set cross-references, and the rest of the review checklist are maintainer concerns on second-pass review — they will redirect rather than reject if those need work.

If you are writing a long-form artifact (multi-page tutorial, system-model explanation, ADR), or you want the full reasoning behind any of the rules below, follow the depth pointers in each step.

## 1. Pick a folder

The folder picks the doc type. The doc type sets the structure.

| Reader intent | Folder | Doc type |
| --- | --- | --- |
| "Walk me through learning X." | `docs/tutorials/` | Tutorial |
| "I have a goal — give me the recipe." | `docs/how-to/` | How-To |
| "What does each option/flag do?" | `docs/reference/` | Reference |
| "Help me understand X / why X is so." | `docs/explanation/` | Explanation |
| "How's this part of Lore actually built?" (contributor) | `docs/developing/internals/` | Internals |
| "Why did the project choose X?" (contributor) | `docs/developing/decisions/` | ADR |
| "How do I write code that conforms to Lore conventions?" (contributor) | `docs/developing/code-standards/` | Code-Standard |

If you are unsure which folder applies, open a draft pull request with your best guess; maintainers will redirect.

Depth: [`canon/doc-types.md`](canon/doc-types.md) defines all seven types, their required sections, and a worked example for "I added a feature — what do I write?"

## 2. Copy the template

Each user-facing doc type has a template file at the folder root:

| Folder | Template |
| --- | --- |
| `docs/tutorials/` | [`tutorial-template.md`](../../tutorials/tutorial-template.md) |
| `docs/how-to/` | [`how-to-template.md`](../../how-to/how-to-template.md) |
| `docs/reference/` | [`reference-template.md`](../../reference/reference-template.md) |
| `docs/explanation/` | [`explanation-template.md`](../../explanation/explanation-template.md) |
| `docs/developing/decisions/` | [`adr-template.md`](../decisions/adr-template.md) |
| `docs/developing/internals/` | Inline in [`canon/doc-types.md`](canon/doc-types.md) § Internals § Template |
| `docs/developing/code-standards/` | Inline in [`canon/doc-types.md`](canon/doc-types.md) § Code-Standard § Template |

Copy, rename to a lowercase, hyphenated filename describing the topic (`resolving-merge-conflicts.md`, not `Resolving Merge Conflicts.md`).

## 3. The must-haves

Five rules cover most of what a small contribution has to get right. Linters catch the rest.

**Sentence case in headings.** "Resolving merge conflicts," not "Resolving Merge Conflicts."

**Code fences with language tags.** Always fenced, never indented:

````markdown
```bash
lore stage src/main.rs
```
````

Tag every fence — `bash`, `rust`, `text`, `console`, `markdown`, and the rest.

**Link text describes the destination.** Never `click here`, `here`, `this`, `read more`, or `link`. Vale flags these.

**Use Lore vocabulary, not Git or Perforce terms.** `commit`, `revision`, `branch`, `clone`, `latest`, `sync`, `working tree`, `stage` — not `HEAD`, `working copy`, `changelist`, `depot`. Vale catches the common substitutions.

**Call the category "version control."** Lore is a version control system, not "revision control." Write "Lore version control" when the project needs to be named unambiguously; plain `Lore` is fine once context is set.

Depth: [`canon/format.md`](canon/format.md) for page shape (headings, lists, tables, callouts), [`canon/language.md`](canon/language.md) for words (voice, contractions, vocabulary, branding, link conventions), [`operational/filenames.md`](operational/filenames.md) for filename rules.

## 4. Run the linters

Before submitting:

```bash
bash scripts/docs-lint.sh
```

The wrapper runs Vale, markdownlint-cli2, and lychee with the canonical arguments and prints a per-tool summary. If you have only some of the three installed, the wrapper warns about the missing ones and runs the rest — submit anyway, and maintainers will run the full set on review.

Depth: [`tools/README.md`](tools/README.md) for install instructions, full rule lists, and per-tool invocations.

## 5. Submit

Open a pull request. In the description, name the folder you targeted, link any related Lore feature or PR, and call out anything you are unsure about — the right doc type, where to put a section, what to cut.

## What maintainers handle

You don't need to walk these yourself for a small contribution. Maintainers will review and redirect:

- Voice and tone consistency across the doc set.
- Required-section sequence and audience pitch (if structurally wrong, they will redirect to the right type).
- Accessibility checks (alt text, screen-reader pass, color use).
- Information architecture — where the page fits in the navigation tree, which Landing it appears on.
- Open-source posture — no internal hostnames, internal ticket IDs, or sponsor-org tooling.
- Cross-references to neighboring docs and Landing-page entries.
- The full pre-publish review checklist.

The full bar a doc clears before publish is in [`operational/review-checklist.md`](operational/review-checklist.md). Most items are maintainer concerns; you don't need to walk them yourself for a small contribution.

## Going deeper

For substantial contributions, or to understand the why behind any rule above:

- [`canon/doc-types.md`](canon/doc-types.md) — all seven doc types, required structure, voice, length, templates.
- [`canon/format.md`](canon/format.md) — headings, lists, tables, code formatting, callouts, key combinations, em-dash spacing.
- [`canon/language.md`](canon/language.md) — voice, mood, contractions, pronouns, Lore vocabulary, banned phrases, branding, link conventions.
- [`operational/filenames.md`](operational/filenames.md) — filenames, slugs, SEO basics.
- [`operational/accessibility-and-media.md`](operational/accessibility-and-media.md) — alt text, color, diagrams, image conventions.
- [`operational/review-checklist.md`](operational/review-checklist.md) — pre-publish checklist (mostly maintainer concerns).
- [`tools/README.md`](tools/README.md) — Vale, markdownlint, lychee.
