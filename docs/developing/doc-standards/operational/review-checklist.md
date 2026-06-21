# Pre-publish review checklist

Walk only the sections that apply to your change — each section states when it triggers.

**Before you start:** Run all three linters (`bash scripts/docs-lint.sh`) and resolve every finding. The items below cover what the linters can't catch. See [`../tools/README.md`](../tools/README.md) for what each tool covers.

- **Typo or copy edit:** [Always](#always) only.
- **Adding or revising prose:** [Always](#always) + [Any prose change](#any-prose-change).
- **New doc or substantial new section:** Above + [New doc or substantial new section](#new-doc-or-substantial-new-section).
- **Tutorial or How-To page:** Above + [Tutorial or How-To pages](#tutorial-or-how-to-pages).
- **Doc includes images, diagrams, or video:** Add [When media is present](#when-media-is-present).
- **ADR — new or status update:** [Architectural decision record](#architectural-decision-record--new-or-status-update) only.

## Always

The minimum bar for any change.

- [ ] All three linters pass. Run via `bash scripts/docs-lint.sh` (or each tool individually — see [`../tools/README.md`](../tools/README.md)).
- [ ] Final QC pass done in rendered (preview) view, not source view — everything renders correctly.
- [ ] If you added or moved a file: it's reachable from the navigation tree (its doc-type folder Landing or a journey manifest entry).

## Any prose change

Triggers on anything beyond a typo fix. Skip if your change is a single typo or copy edit.

- [ ] Voice and tone are consistent with the surrounding doc. *(See [language.md § Voice](../canon/language.md#voice).)*
- [ ] Mood matches the surrounding context (imperative for steps, indicative for descriptions). *(See [language.md § Mood](../canon/language.md#mood).)*
- [ ] Specialized terms are spelled and capitalized consistently with the rest of the doc set.
- [ ] The category is called "version control," not "revision control" (the Lore primitive `revision` is unaffected). *(See [language.md § Category naming](../canon/language.md#category-naming).)*
- [ ] Inline code is wrapped in backticks (file paths, command names, identifiers, environment variables). *(See [format.md § Code formatting](../canon/format.md#code-formatting).)*
- [ ] Code snippets are formatted correctly for the language. *(See [format.md § Code formatting](../canon/format.md#code-formatting).)*
- [ ] Tables are used for tabular data, not for layout. *(See [format.md § Tables](../canon/format.md#tables).)*
- [ ] Callout type matches the content — a `WARNING` is a real risk, not a tip. *(See [format.md § Callouts and admonitions](../canon/format.md#callouts-and-admonitions).)*
- [ ] Internal links use relative paths (not absolute URLs into the same site). *(See [language.md § Cross-references](../canon/language.md#cross-references).)*
- [ ] Third-party doc links use the verbose-inline-parenthetical or full-standalone-cross-reference pattern. *(See [language.md § Third-party documentation links](../canon/language.md#third-party-documentation-links).)*
- [ ] Em and en dashes have a space before and after. *(See [format.md § Em dashes, en dashes, hyphens](../canon/format.md#em-dashes-en-dashes-hyphens).)*
- [ ] `we` / `our` / `us` speaks for the Lore project (decisions, recommendations, history, what the project provides) and isn't instructional. *(See [language.md § Pronouns](../canon/language.md#pronouns).)*

### Open-source posture

*Lore is open-source and the docs are public-facing. See the README § Scope for the framing.*

- [ ] No internal hostnames, internal Slack channels, internal ticket-tracker IDs, internal wiki links, or internal infrastructure names.
- [ ] No references to private cloud accounts, internal cluster names, or sponsor-organization tooling (unless the page explicitly covers the project's reference deployment).
- [ ] No code that depends on private dependencies or repositories.
- [ ] Maintainer voice is collective and project-scoped, not corporate. `we` is for Lore-project recommendations, not on behalf of a sponsor organization.

## New doc or substantial new section

Triggers when the contribution adds new ground rather than extending existing prose. Skip when you are touching only a small section of an existing page.

- [ ] Page is the right doc type for its reader intent. (A "tutorial" that pivots to a flag table halfway through is two docs.) *(See [doc-types.md § Choose the right doc type](../canon/doc-types.md#choose-the-right-doc-type).)*
- [ ] Page follows the required H2 sequence for its type. *(See [doc-types.md](../canon/doc-types.md).)*
- [ ] Information is well-organized with a logical flow.
- [ ] Heading levels reflect document structure, not visual size preferences. *(See [format.md § Hierarchy](../canon/format.md#hierarchy).)*
- [ ] Headings break up text in a way that supports scanning. *(See [format.md § Headings](../canon/format.md#headings).)*
- [ ] Headings at the same level use parallel grammatical construction. *(See [format.md § Parallel construction](../canon/format.md#parallel-construction).)*
- [ ] Paragraphs are chunked, not dense walls of text. *(See [format.md § Chunking](../canon/format.md#chunking).)*
- [ ] Audience level is appropriate (not over- or under-explaining). *(See [language.md § Audience](../canon/language.md#audience).)*
- [ ] No idiomatic language that doesn't translate. *(See [language.md § No idioms or slang](../canon/language.md#no-idioms-or-slang).)*
- [ ] Filler words and phrases that can be removed without losing meaning are deleted.
- [ ] Cross-references to related Lore docs are surfaced where the reader will need them. *(See [language.md § Cross-references](../canon/language.md#cross-references).)*
- [ ] Glossary terms (when a glossary exists) are linked to their definitions on first use; plain text after.

## Tutorial or How-To pages

Triggers when the page is under `docs/tutorials/` or `docs/how-to/`. Skip otherwise.

- [ ] Imperative mood for steps ("Run," "Open," "Set"), not "You should run." *(See [language.md § Mood](../canon/language.md#mood).)*
- [ ] A reader on the public internet can follow the page end-to-end with only the public Lore release.
- [ ] Multi-page tutorials: child-page splits happen at logical points and parent/child relationships are correct. *(See [doc-types.md § Tutorial](../canon/doc-types.md#tutorial).)*

## When media is present

Triggers when the change adds, modifies, or relies on an image, diagram, or video. Skip if no media is involved.

- [ ] Alt text describes meaningful content, not what's depicted. *(See [accessibility-and-media.md § Alt text](accessibility-and-media.md#alt-text).)*
- [ ] Alt text doesn't begin with "Image of" or with "Picture of." *(See [accessibility-and-media.md § Alt text](accessibility-and-media.md#alt-text).)*
- [ ] No content conveys information by color alone. *(See [accessibility-and-media.md § Don't use color alone](accessibility-and-media.md#dont-use-color-alone).)*
- [ ] Page passes a screen-reader pass (read aloud through Narrator, VoiceOver, or Orca). *(See [accessibility-and-media.md § Publish-time self-check](accessibility-and-media.md#publish-time-self-check).)*

## Architectural decision record — new or status update

Triggers when the change adds a new ADR or updates the `status` or `date` fields of an existing one. Skip for changes outside `docs/developing/decisions/`.

*See [`../canon/doc-types.md`](../canon/doc-types.md) § ADR.*

- [ ] New ADR: uses a `status` / `date` header block (not a title/description block).
- [ ] New ADR: filename follows `NNNNN-<slug>.md` with the next available sequence number; no number is reused.
- [ ] Status update on an accepted ADR: only `status` and `date` fields changed — body is untouched.
- [ ] Change of decision: a new ADR is written (not an edit to the existing body); the prior ADR's `status` is updated to `superseded by ADR-NNNNN` and its body is left unchanged.
- [ ] Change of decision: the new ADR links to the prior (superseded) ADR in its `More Information` section.

## Process (recommended for substantial new work)

Not gating; not required for a typo fix or small addition. Substantial new docs benefit from these.

- [ ] Doc has had a self-edit pass (let it sit, return with fresh eyes).
- [ ] Doc has had a second-set-of-eyes review.
- [ ] Long docs reviewed in chunks rather than a single pass.
- [ ] Multiple passes done with different focus each time (structure, language, accessibility).
