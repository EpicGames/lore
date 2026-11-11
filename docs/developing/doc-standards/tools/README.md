---
title: Doc-standards tools
description: The three linters that maintain Lore documentation quality — Vale (prose), markdownlint (structure), lychee (links) — with usage, install, and rule details.
---

# Doc-standards tools

Three linters maintain Lore documentation quality. The tool configs live next to this README under `tools/<tool>/` so editor plugins discover them without configuration; this file documents how to invoke each tool, what it catches, and how to install it.

| Tool | What it catches | Config location |
| --- | --- | --- |
| [Vale](#vale-prose-linting) | Prose: banned phrases, Git-isms, link text, product naming, sentence-case headings, and more. | `.vale.ini` (repo root) + `tools/vale/styles/` |
| [markdownlint](#markdownlint-structural-linting) | Structure: heading hierarchy, fence-tag presence, list rules, table mechanics. | `.markdownlint-cli2.jsonc` (repo root) |
| [lychee](#lychee-link-integrity) | Links: every internal and external link in the docs resolves. | `tools/lychee/lychee.toml` |

## Run all three

The recommended pre-publish invocation:

```bash
bash scripts/docs-lint.sh
```

The script runs Vale, markdownlint-cli2, and lychee in sequence with the canonical arguments, prints a section per tool, continues through all three even when one fails (so you see every finding), and prints a summary at the end.

Behavior on missing linters:

- **Some missing.** The script warns on stderr for each missing linter, runs the others, and exits based on findings — the missing linters don't by themselves cause a non-zero exit. Useful for partial-install community contributors.
- **All missing.** The script exits non-zero (code 2) — a run with no signal isn't a clean check, and CI must catch a broken image.

Exit codes: `0` if every linter that ran was clean; `1` if a linter reported findings and none errored; `2` if a linter errored (couldn't run to completion) or no linter is installed locally. The end-of-run summary labels each tool `passed`, `FINDINGS`, `ERRORED`, or `missing` so a crash is never mistaken for a clean pass.

If you want to invoke a tool directly — for a scoped scan, alternate args, or to debug — see the per-tool sections below.

## Vale prose linting

[Vale](https://vale.sh/) is a syntax-aware prose linter. The Lore Vale config uses a single `Lore` style package authored in this repo, incorporating rules derived from the Microsoft Writing Style Guide alongside Lore-specific rules. Two style-guide rules are disabled because Lore diverges from them; each divergence is documented at its topical home in [`canon/format.md`](../canon/format.md) (em-dash spacing) and [`canon/language.md`](../canon/language.md) (project-voice "we").

### Recommended invocation

```bash
vale --glob='!*-template.md' docs/
```

Auto-discovers `.vale.ini` at the repo root. The `--glob` exclusion skips `*-template.md` scaffolding — the same files lychee excludes (see [Scope](#scope)). A root `README.md` joins the target set once it exists.

> [!TIP]
> Use `bash scripts/docs-lint.sh` as the standard pre-publish command. The script automatically expands the target set when additional doc trees are present.

### What it catches

**Lore package**: Lore-specific rules plus ~30 rules derived from the Microsoft Writing Style Guide, covering capitalization (sentence-case headings), gender-neutral language, Oxford comma, hyphenation, weasel words, passive-voice flags, and wordiness. Two style-guide rules are disabled as documented divergences: `Lore.Dashes`, `Lore.We`. Each rule's `link:` field points to the canonical prose section, so a writer hitting a finding clicks through to the rule's home.

| Rule | Catches | Severity | Documented in |
| --- | --- | --- | --- |
| `Lore.Difficulty` | Difficulty-claim register: `Just <verb>` / `simply <verb>` imperative softeners, `easily <verb>` / `easily <X>-able` passive ease claims, `all you have to do is`, bare `obviously` | warning | [`language.md` § Difficulty descriptors](../canon/language.md#difficulty-descriptors) |
| `Lore.PermissionVerbs` | `allows you`, `lets you`, `permits the user`, `enables users` | warning | [`language.md` § Permission verbs](../canon/language.md#permission-verbs) |
| `Lore.LinkText` | `[click here]`, `[here]`, `[this]`, `[read more]`, `[link]` as link text | error | [`language.md` § Link text](../canon/language.md#link-text) |
| `Lore.Dropdown` | `drop-down`, `drop down` → `dropdown` | warning | [`language.md` § Other terms to avoid](../canon/language.md#other-terms-to-avoid) |
| `Lore.ClickOn` | `click on`, `right-click on`, `double-click on` → drop the trailing `on` | warning | [`language.md` § Action verbs](../canon/language.md#action-verbs) |
| `Lore.AndOr` | `and/or`, `either/or` (rewrite to `and` or `or`) | warning | [`language.md` § Other terms to avoid](../canon/language.md#other-terms-to-avoid) |
| `Lore.Boolean` | lowercase `boolean` / `booleans` → `Boolean` / `Booleans` | warning | [`language.md` § Other terms to avoid](../canon/language.md#other-terms-to-avoid) |
| `Lore.VsAbbrev` | `VS`, `vs.`, `VS.` → `vs` (lowercase, no period) | warning | [`language.md` § Other terms to avoid](../canon/language.md#other-terms-to-avoid) |
| `Lore.HyphenLy` | hyphenated `-ly` adverb compounds (`fully-qualified`) | warning | [`format.md` § Hyphenation](../canon/format.md#hyphenation) |
| `Lore.GitIsmSubstitutions` | `HEAD` → `latest`, `working copy` → `working tree`, `git index` → `stage`, `git pull`/`git fetch` → `lore sync` | warning | [`language.md` § Lore vocabulary, not Git-isms](../canon/language.md#lore-vocabulary-not-git-isms) |
| `Lore.GitIsmFlagged` | `detached HEAD`, `git stash`/`stashed`/`stashing`, `refspec`, `reflog`, `submodule` (needs writer judgment — no analog or multiple analogs depending on intent) | warning | [`language.md` § Lore vocabulary, not Git-isms](../canon/language.md#lore-vocabulary-not-git-isms) |
| `Lore.PerforceIsmSubstitutions` | `changelist` → `revision`, `p4 integrate` → `lore branch merge`, `p4 sync` → `lore sync`, `p4 submit` → `lore commit`, `p4 revert` → `lore reset` | warning | [`language.md` § Lore vocabulary, not Git-isms](../canon/language.md#lore-vocabulary-not-git-isms) |
| `Lore.PerforceIsmFlagged` | `depot`, `depot path`, `shelve`/`unshelve`, `p4 client`, `p4 stream` (no Lore analog) | warning | [`language.md` § Lore vocabulary, not Git-isms](../canon/language.md#lore-vocabulary-not-git-isms) |
| `Lore.MkDocsAdmonitions` | `!!! note`, `!!! tip`, `!!! warning` (legacy MkDocs syntax) | error | [`format.md` § Callouts and admonitions](../canon/format.md#callouts-and-admonitions) |
| `Lore.MkDocsTabs` | Raw pymdownx tab syntax (`=== "..."`, `/// tab \|`) instead of the comment-delimited tab convention | error | [`format.md` § Content tabs](../canon/format.md#content-tabs) |
| `Lore.AlertTypes` | Non-GFM alert types: `> [!INFO]`, `> [!DANGER]` (the five GFM-native types — `NOTE`, `TIP`, `IMPORTANT`, `WARNING`, `CAUTION` — are all allowed) | warning | [`format.md` § Callouts and admonitions](../canon/format.md#callouts-and-admonitions) |
| `Lore.ProductNaming` | `the Lore tool`, `the Lore CLI` | warning | [`language.md` § Lore product naming](../canon/language.md#lore-product-naming) |
| `Lore.AmericanSpelling` | `cancelling`, `colour`, `defence`, `whilst`, `backwards`, `towards`, +14 others | warning | [`language.md`](../canon/language.md) |
| `Lore.BlackWhiteList` | `blacklist` → `denylist`, `whitelist` → `allowlist` | error | [`language.md` § Other terms to avoid](../canon/language.md#other-terms-to-avoid) |
| `Lore.NativeCode` | `native code` → `unmanaged code` | warning | [`language.md` § Other terms to avoid](../canon/language.md#other-terms-to-avoid) |
| `Lore.LatinAbbrev` | `i.e.`, `e.g.`, `etc.`, `cf.`, `n.b.` | warning | [`language.md` § No Latin abbreviations](../canon/language.md#no-latin-abbreviations) |

The link paths in each rule's YAML are repo-relative. They render as clickable paths in editor plugins. Once the doc site is live, batch-substitute to absolute URLs.

### Scope

Vale lints prose only. The config excludes:

- **Code spans** (` `inline` `) and inline code formatting (`tt`).
- **Code blocks** (` ``` ` fenced or indented).
- **YAML frontmatter** at the top of every doc.

A code block containing `simply` doesn't fire `Lore.Difficulty`, and frontmatter values don't get linted.

**Template files** (`*-template.md`) are skipped too, but at the invocation level (the `--glob` flag above) rather than by `.vale.ini`: their placeholder and boilerplate content (`{title of option 1}`, illustrative `e.g.`, MADR section headings) isn't authored prose. lychee excludes the same files.

### Severity

`MinAlertLevel = warning` surfaces both errors and warnings; suggestions are quiet by default. Each `Lore.*` rule sets its own `level:` based on how Lore weighs the underlying rule.

### Project vocabulary

Vale's bundled English wordlist is conservative — it flags morphologically derived words (`reframed`, `relitigating`, `deduplicated`, `lowercased`), plurals of compound nouns (`hostnames`), and domain jargon (`lede`, `frontmatter`, `ADRs`) that Lore docs use intentionally.

The Lore project vocabulary lives at `tools/vale/styles/config/vocabularies/Lore/accept.txt` (one term per line). `.vale.ini` loads it via the `Vocab = Lore` directive. To accept a new technical term, add it to that file and re-run Vale; no code changes elsewhere.

Add a term only when it's a real word the docs use intentionally. If you find yourself adding misspellings, fix the prose instead.

### Install

```bash
brew install vale
```

Or download a release binary from <https://github.com/errata-ai/vale/releases>. Verify:

```bash
vale --version
```

## markdownlint structural linting

[markdownlint-cli2](https://github.com/DavidAnson/markdownlint-cli2) catches structural issues in Markdown — heading hierarchy, fence-tag presence, list ordering, table structure, multiple H1s, and similar mechanics.

### Recommended invocation

```bash
markdownlint-cli2 "docs/**/*.md" "README.md"
```

Auto-discovers `.markdownlint-cli2.jsonc` at the repo root. No `--config` flag needed.

> [!TIP]
> Use `bash scripts/docs-lint.sh` as the standard pre-publish command. The script automatically expands the target set when additional doc trees are present.

### What it catches

The config enables markdownlint's full default ruleset. The most useful rules for Lore docs:

| Rule | Catches |
| --- | --- |
| `MD001` | Heading levels skip (H2 → H4 with no H3). |
| `MD003` | Mixed heading styles (Setext vs ATX). |
| `MD009` | Trailing whitespace. |
| `MD012` | Multiple consecutive blank lines. |
| `MD022` | Headings not surrounded by blank lines. |
| `MD025` | More than one H1 in a page. |
| `MD031` | Fenced code blocks not surrounded by blank lines. |
| `MD034` | Bare URLs that should be Markdown links. |
| `MD040` | Code fences without a language tag. |
| `MD046` | Indented code blocks (use fenced). |
| `MD048` | Inconsistent fence style. |
| `MD056` | Tables with mismatched column counts. |

Full rule list: <https://github.com/DavidAnson/markdownlint/blob/main/doc/Rules.md>.

### Rule choices

The Lore config disables two rules and tweaks two. Every other rule is at its default.

| Rule | Setting | Why |
| --- | --- | --- |
| `MD013` (line length) | disabled | Lore docs don't enforce a hard wrap. Prose wraps for the reader's editor. |
| `MD033` (no inline HTML) | disabled | Templates use HTML comments (`<!-- delete after copying -->`). Lore docs author no other inline HTML. |
| `MD024` (no duplicate headings) | `siblings_only: true` | Type templates legitimately repeat H3s ("Definition," "When to Use It") across H2 sections. Duplicates within the same H2 still fail. |
| `MD025` (single top-level heading) | `front_matter_title: ""` | The Lore standards require a body H1 that matches frontmatter `title`. markdownlint's default counts the frontmatter title as a heading and flags the body H1 as a duplicate. Disabling the frontmatter scan keeps the rule enforcing "one body H1 per page" without false-firing on the matched pair. |

### Install

```bash
brew install markdownlint-cli2
```

Linux, Windows, or any environment without Homebrew:

```bash
npm install -g markdownlint-cli2
```

Verify:

```bash
markdownlint-cli2 --version
```

### Output

By default, markdownlint-cli2 prints a human-readable list of findings: file, line, rule, message. The exit code is non-zero if any rule fires.

For tooling integrations:

```bash
markdownlint-cli2 --output-format json "docs/**/*.md"
```

## Lychee link integrity

[Lychee](https://github.com/lycheeverse/lychee) validates that every link in the Lore documentation resolves: internal relative paths, anchors, and external URLs.

### Recommended invocation

To check the whole documentation tree:

```bash
lychee --config docs/developing/doc-standards/tools/lychee/lychee.toml docs/
```

This is the **only invocation that catches every kind of breakage**, including links broken by moving or renaming a file. Use it when you've moved a doc, renamed a page, restructured a folder, or want to know whether the docs are clean.

> [!TIP]
> Use `bash scripts/docs-lint.sh` as the standard pre-publish command. The script automatically expands the target set when additional doc trees are present.

### How lychee scans

Lychee is a one-way scanner. It reads the inputs you give it, extracts every link in those files, and verifies each one resolves. It has no reverse index — no concept of "what links to this file?"

That means:

- A scan of a single file catches only *outgoing* breakage from that file. If you renamed `docs/explanation/old-name.md` to `new-name.md`, scanning only `new-name.md` won't tell you that `docs/tutorials/foo.md` still links to the old path.
- To catch breakage from moves and renames (by far the most common cause of broken links), scan the whole tree. This is what the recommended invocation above does.

If you only want to check the links *outgoing from* a doc you're currently editing, see [Advanced: scoped scans](#advanced-scoped-scans). Be aware of what it doesn't cover.

### Output

By default, lychee prints a human-readable summary. To produce machine-parseable output (useful for tooling integrations):

```bash
lychee --config docs/developing/doc-standards/tools/lychee/lychee.toml \
       --format json --output - docs/
```

The cache lives at `.lycheecache` in the repo root and persists between runs. The first whole-tree pass is ~1–2 minutes against the current doc set; subsequent passes return in seconds when the cache is warm.

### No internal-domain allowlist

Lore docs under `docs/` are public-facing. The `exclude` list in `lychee.toml` does **not** include internal-org hostnames (sponsor-org GitHub Enterprise, internal wikis, internal infrastructure subdomains). An internal link appearing under `docs/` is a failure by design — replace it with a public link or remove it before merge.

The `exclude` list only carries non-org-revealing entries: `localhost`/loopback (tutorials reference these intentionally), `example.com` (canon samples), and the access-gated Unreal Engine source repository on GitHub, which returns 404 until the reader links their Epic Games and GitHub accounts. Internal doc trees not present in the public repository are handled automatically by `scripts/docs-lint.sh`.

### Advanced scoped scans

If you only want to validate the outgoing links of a single file you've recently edited, and you accept that no backlinks elsewhere will be checked:

```bash
lychee --config docs/developing/doc-standards/tools/lychee/lychee.toml docs/path/to/your-doc.md
```

It doesn't catch breakage from *other* docs whose links may now be stale. Don't use it as a standalone before-merge check.

### Install

Lychee isn't vendored. Install it with one of:

```bash
brew install lychee
cargo install lychee
```

Then verify:

```bash
lychee --version
```

If link-checking moves into CI, set the `GITHUB_TOKEN` environment variable so the GitHub API isn't rate-limited; lychee picks it up automatically.

## Adding a tool

When adding a new linter:

1. Create `tools/<tool>/` for the tool's config (and any auxiliary files).
2. Add a section to this README following the same shape: recommended invocation, what it catches, install, scope/config notes.
3. Update the table at the top of this file.
4. If automation consumes the tool, document the integration point alongside that automation.
