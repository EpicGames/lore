# Format

The "what shape the page takes" half of the standards: headings, lists, tables, bold/italic/code, callouts, and key combinations. Voice, mood, word choice, and links live in [`language.md`](language.md).

## Headings

### Hierarchy

Every page has exactly one H1. It's the page title, in sentence case. The H1 ensures a visible title on every reading surface (GitHub, editors, and the rendered site). Body sections below the H1 start at H2.

Default top body level is H2; subsections are H3. Avoid H4 unless the subject genuinely needs a fourth level. Don't use H5 or H6.

A deep heading tree produces an unreadable right-rail TOC. If a section needs H4+, it's probably long enough to be its own page.

### Capitalization — sentence case

Use **sentence case** for every heading: capitalize the first word and any proper nouns; lowercase everything else.

- Right: `## Stage and commit files`
- Right: `## Configure the Lore CLI` *(Lore is a proper noun; CLI is an acronym)*
- Wrong: `## Stage and Commit Files`

Vale `Lore.Headings` enforces.

### Parallel construction

Headings at the same level should share a parallel grammatical structure. Pick one shape per level — gerund, imperative, or noun phrase — and apply it consistently across siblings.

```markdown
<!-- correct: all gerund -->
## Cloning a repository
## Branching from main
## Merging into main
```

```markdown
<!-- correct: all imperative -->
## Clone a repository
## Branch from main
## Merge into main
```

```markdown
<!-- wrong: mixed -->
## Cloning a repository
## How to branch
## Merge
```

### Headings are signposts, not summaries

Keep headings short. Don't put parenthetical acronym expansions in headings — use either the acronym or the spelled-out form, then expand on first use in the body.

- Right: `## Continuous integration`
- Wrong: `## Continuous integration (CI)`

## Chunking

Densely packed text is hard to scan. White space increases readability. Techniques: short paragraphs, simple sentences, lists, tables.

- Prefer short, simple sentences.
- More than two commas in a sentence is a signal to split.
- Don't link more than two phrases or clauses with `and`, `or`, or `but`.

## Lists

Two kinds: **bulleted (unordered)** and **numbered (ordered)**.

- Use bulleted lists when sequence doesn't matter.
- Use numbered lists when sequence matters.
- Use a table when each item has the same set of attributes.
- Use prose when there are fewer than three items and the sequence flows.

### Bulleted list rules

- Capitalize the first word of every item.
- End the introductory sentence with a colon.
- If any item is a complete sentence, end every item with a period. If no item is a complete sentence, omit end punctuation.
- Don't use semicolons or commas at the ends of items. Periods or nothing.
- Start every item with the same part of speech where possible.
- Make every item either a full sentence or a sentence fragment. Don't mix.
- Items should be of similar length. A two-word item next to a paragraph-long item is a signal the list is doing two different jobs.

### Heading-style bullets — bold + colon or em dash

Bold the leading term. Separate it from the explanation with a colon (`:`) or an em dash (`—`). Don't use a hyphen as the separator.

```markdown
<!-- correct -->
- **Commit:** A snapshot of the repository at a point in time.
- **Branch —** A movable pointer to a commit.
```

```markdown
<!-- wrong -->
- **Commit** - snapshot
```

### Numbered list rules

- Use a numbered list for any procedure with three to ten steps.
- Fewer than three steps: use prose or a bulleted list.
- More than ten steps: split into sub-steps under numbered parents, or break into multiple topics.
- Capitalize the first word of every step.
- End every step with a period unless every step is a sentence fragment.
- Start every step with an imperative verb (`Run`, `Open`, `Set`).

## Tables

- Always include a header row.
- Bold the leftmost-column key terms (these are the row labels readers scan first). Exception: if the term is a command, variable, or function name, use code formatting (backticks) instead.
- Keep cell content short. If a cell needs paragraphs, the table is the wrong shape — use a section per item instead.
- Use tables for tabular data, not for layout.

```markdown
| Setting           | Description                                                  | Default |
| ------------------- | -------------------------------------------------------------- | --------- |
| **Auto Connect**  | Connect to the default server automatically on open.         | `false` |
| **Default Server**| Server URL the system tries to connect to on **Go Live**.    | none    |
```

## Bold

Use bold for:

- UI element names — button names, menu items, option names.
- Key strokes: `Press **Esc**.`
- Option names in tables and prose.
- Default value names.
- Menu paths — bold the entire path with `>` separators: `**File > Preferences > Editor**`.
- Key terms in tables (usually the leftmost column).
- First instance of a newly introduced term in body prose.

Don't bold:

- The Lore product name in body text.
- Links — Markdown link styling is built in.

## Italics

Three legitimate uses: titles of referenced books or external works; a term being introduced for the first time on a page (alternative to bold — pick one and stay consistent within a doc); a foreign-language word or a heading name being referenced inside a third-party doc.

Don't use italics or quotation marks for general emphasis. Use bold.

- Right: `The **read-only** flag prevents writes.`
- Wrong: `The "read-only" flag prevents writes.`

## Underline

Never. Underlined text is reserved for hyperlinks, which the renderer styles automatically.

## Code formatting

Use **fenced code blocks** with a language tag for all code examples.

````markdown
```bash
lore stage src/main.rs
lore commit "Add main entry point"
```
````

Use `bash` for shell commands the reader runs. Use `console` for interactive terminal sessions that show a prompt alongside output. Use `text` for plain output with no executable commands.

Use **inline code spans** (single backticks) for command names (`lore status`), file paths (`~/.config/lore/config.toml`), flag names (`--force`), environment variables (`LORE_HOME`), function names, and option keys.

Don't bold or italicize code. Don't wrap product names (`Lore`) or UI element names in code spans — use bold for UI; plain text for the product.

Don't bake text into images of code. Always use a code block, never a screenshot of a terminal.

## Callouts and admonitions

Lore docs use **GitHub Flavored Markdown alert syntax**. Use the five GFM-native types and pick the one that matches the content.

| Type | When to use | Decision rule |
| --- | --- | --- |
| `TIP` | Optional shortcut, productivity hint, or nice-to-know context. The reader can skip it with no loss. | The content is *helpful* but not required. |
| `NOTE` | Useful context that belongs on the page but interrupts flow. The reader benefits from seeing it but the page works without it. | The content is *informative* — neither optional nor required. |
| `IMPORTANT` | Information the reader must take in to succeed at the task on the page. Skipping leads to failure or confusion, not damage. | The content is *required for success*. |
| `CAUTION` | An action with a recoverable downside or a non-obvious side effect. A heads-up before the reader acts. | The content describes a *recoverable risk* — think before acting. |
| `WARNING` | An action causing data loss, security exposure, or other unrecoverable damage. The reader must heed it. | The content describes an *unrecoverable risk*. |

Decision tree, top-down:

1. **Is it about a risk of damage?** No → `TIP` (optional), `NOTE` (informative), or `IMPORTANT` (required for success). Yes → `CAUTION` (recoverable) or `WARNING` (unrecoverable).
2. **Is it required to succeed at the task?** Yes → `IMPORTANT`. No → `NOTE` if the reader benefits from seeing it, `TIP` if they can skip it.
3. **Is the consequence recoverable?** Yes → `CAUTION`. No → `WARNING`.

```markdown
> [!TIP]
> Use `lore history --oneline` to scan a branch's recent revisions.

> [!NOTE]
> Lore revisions are content-addressed. The hash signature is computed
> from the content, so two identical revisions share one signature.

> [!IMPORTANT]
> Run `lore sync` before branching, or your new branch will start from
> a stale revision and the merge will conflict against current state.

> [!CAUTION]
> `lore branch archive` removes the local branch pointer. Unmerged
> revisions remain reachable through the reflog for 30 days, after
> which garbage collection prunes them.

> [!WARNING]
> `lore reset --hard` discards uncommitted changes. There is no undo.
```

Don't use `INFO` or `DANGER` — they aren't GFM-native and won't render on GitHub. Vale `Lore.AlertTypes` flags them.

Don't use the legacy MkDocs `!!! note` / `!!! tip` / `!!! warning` syntax — it doesn't render on GitHub or in editors and locks the docs to a single static-site generator. Vale `Lore.MkDocsAdmonitions` enforces.

Don't hide load-bearing instructions or warnings inside collapsed sections.

Match content to type. A `WARNING` that's actually a tip cries wolf; an `IMPORTANT` used for nice-to-know dilutes the ones that matter. When in doubt between two adjacent types (`NOTE` vs `IMPORTANT`, `CAUTION` vs `WARNING`), pick the *less alarming* one — escalation is meaningful only if you reserve it.

## Material for MkDocs features

Lore docs render on two surfaces: GitHub (raw Markdown) and the published site themed with [Material for MkDocs](https://squidfunk.github.io/mkdocs-material/). The default is syntax that works on both — that's why the admonitions section above mandates GFM alerts instead of Material's `!!! note`. This section covers the narrow case where you reach past that default for a Material-only feature.

### What the theme enables

`mkdocs.yml` enables a small set of extensions and theme features. The ones that affect authoring are:

| Feature | Extension or theme flag | What it gives you |
| --- | --- | --- |
| GFM alert rendering | `gfm_admonition` | Converts `> [!NOTE]` / `> [!IMPORTANT]` / `> [!WARNING]` etc. to Material admonitions at parse time. Required for callouts to render as styled boxes on the published site. (`markdown-gfm-admonition` package.) |
| Content tabs | `pymdownx.tabbed` | Side-by-side variants of the same content (for example, per-platform install steps). Authored in the comment-delimited convention below; the `content_tabs.py` hook converts each group at build time. |
| Collapsible blocks | `pymdownx.details` | `???` and `???+` collapsibles. |
| Rich code fences | `pymdownx.superfences` | Nested fences inside lists, tabs, and admonitions. |
| Code copy button | `content.code.copy` | Adds a copy icon to every code block. Free; no syntax change. |
| Code annotations | `content.code.annotate` | Numbered callouts pointing at lines inside a code block. |
| HTML attributes | `attr_list` | Per-element classes and IDs via `{ .class #id }`. |
| HTML in Markdown | `md_in_html` | Lets Markdown inside `<div>` blocks still parse as Markdown. |

Several of these have no GFM equivalent and won't render on GitHub. The copy button and code annotations are free — they cost the reader nothing on GitHub because the syntax is invisible. Tabs, collapsibles, and `attr_list` change the source enough to matter.

### When to reach for a Material-only feature

Ask: *would this doc be worse without it?* If the alternative is a long parallel subsection structure, a hard-to-scan list, or three near-duplicate procedures, a Material feature probably earns its keep. If the alternative is a single short list, plain Markdown wins.

A counter-example is admonitions: GFM alerts render on both surfaces and are better than `!!! note`, so the rule there is closed — see the section above. Tabs are now closed too: the comment-delimited convention below renders cleanly on both surfaces, so raw `=== "..."` syntax is banned in source. Vale `Lore.MkDocsTabs` enforces.

### Content tabs

Use content tabs for parallel content that the reader will pick exactly one of: per-platform install steps, per-language code samples, per-audience walkthroughs. The reader picks one tab; the others stay hidden.

Tabs are authored in a comment-delimited convention, never raw `=== "..."` syntax (Vale `Lore.MkDocsTabs` enforces). GitHub renders the comments as nothing and the titles as bold text, so the source stays readable on both surfaces; the `content_tabs.py` hook converts each group to Material tabs when the site builds.

``````markdown
<!-- tabs:start -->

<!-- tab -->
**macOS**

```bash
curl -Lo lore https://example.com/lore-macos
```

<!-- tab -->
**Linux**

```bash
curl -Lo lore https://example.com/lore-linux
```

<!-- tabs:end -->
``````

The hook checks every group at build time — a malformed group is left unconverted and fails `mkdocs build --strict`:

- `<!-- tabs:start -->`, `<!-- tab -->`, and `<!-- tabs:end -->` each sit on their own line, all at the same indentation (inside a list step, the list's content indentation).
- Each `<!-- tab -->` is followed by the tab's title: a line that is only bold text, like `**macOS**`. Titles can't contain double quotes or asterisks.
- A bold-only line without a preceding `<!-- tab -->` is ordinary content — it never starts a tab.
- A group holds two or more tabs. Content is written at its natural indentation — no extra four-space indentation, and code fences stay ordinary fences.

Don't hide load-bearing instructions inside tabs. A reader who needs all platforms (for example, setting up CI on Linux after developing on macOS) shouldn't have to click through every tab to discover what's identical and what differs — if more than half the steps repeat across tabs, pull the shared steps out and tab only the divergent parts.

### Researching further capabilities

Material ships features frequently and training data drifts. When you need a feature not covered here, use the `context7` MCP to read current docs rather than guessing:

1. Call `resolve-library-id` with `libraryName: "Material for MkDocs"`. It returns the Context7 ID `/squidfunk/mkdocs-material`.
2. Call `query-docs` with that ID and a specific question — for example, *"How do content tabs work with nested code blocks?"* or *"What does `content.code.annotate` enable and how are annotations written?"*

Most of the rich Markdown syntax in Material comes from PyMdown Extensions, documented at <https://facelessuser.github.io/pymdown-extensions/> and reachable via Context7 as `/facelessuser/pymdown-extensions`. Tab syntax, collapsibles, snippets, and superfences all live there — query that library when the question is about how the syntax itself works, not how Material styles it.

Before adding a new Material-only feature to a doc, check whether the GFM-friendly syntax covered earlier in this file already does the job. The dual-render bias still wins by default.

## Hyphenation

### Compound modifiers before a noun — hyphenate

`Lore is a large-scale project.` `The team prefers user-centric features.`

### Compounds ending in -ly — don't hyphenate

The `-ly` already signals modification.

- Right: `closely related branches`, `fully qualified path`
- Wrong: `closely-related branches`, `fully-qualified path`

Vale `Lore.HyphenLy` enforces.

### Compound after the noun — no hyphen by default

`The project has a large scale.` `The features are user-centric.`

## Em dashes, en dashes, hyphens

Lore docs use **spaces around em and en dashes**.

| Mark | Width | Use | Spacing |
| --- | --- | --- | --- |
| Em dash (`—`) | width of an `m` | Sets off explanatory text; substitutes for paragraphs, commas, colons | Space before and after |
| En dash (`–`) | half an em | Numeric and date ranges | Space before and after |
| Hyphen (`-`) | keyboard | Compound words | n/a |

A hyphen isn't a valid substitute for an em dash, en dash, or colon.

> [!NOTE]
> Lore uses spaced em and en dashes — spaced dashes read better in proportional fonts and wrap predictably across line widths in editors and on the rendered site. `.vale.ini` disables `Lore.Dashes`.

## Key combinations

When describing key combinations, put a space before and after the `+`.

- Right: `Ctrl + C`, `Alt + Shift + Enter`
- Wrong: `Ctrl+C`, `Alt+Shift+Enter`

Spell out the name of an unusual or ambiguous key when introducing it: ``the backtick (`) key``.

## Keyboard keys

Use the casing printed on a U.S. keyboard. Most keys are initial cap with the rest lowercase: `Esc`, `Ctrl`, `Shift`, `Alt`, `Tab`, `Enter`.

Spell out `Shift`, `Hyphen`, and any ambiguous keys.

`Space bar` is two words.

## Symbols and characters

| Symbol | Meaning |
| --- | --- |
| `—` | Em dash |
| `–` | En dash |
| `©` | Copyright symbol |
| `®` | Registered symbol |
| `™` | Trademark symbol |
| `°` | Degree symbol |

Less common keyboard names useful when describing combinations:

| Key | Name |
| --- | --- |
| `~` | tilde |
| `` ` `` | backtick |
| `[ ]` | left and right brackets |
| `{ }` | left and right curly brackets (also: curly braces) |
| `< >` | left and right angle brackets |
| `^` | caret |
| `*` | asterisk |
| `&` | ampersand |
| `/` | slash (also: forward slash) |
| `\` | backslash |
| `\|` | pipe (also: vertical bar) |

## Reusing content across pages

When the same content appears on more than two pages, factor it into a single source-of-truth page and link to it from the others. For one or two occurrences, inline the content. Pick one of inline or link-out, and stay consistent within the doc set.
