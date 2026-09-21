# Language

The "what words go on the page" half of the standards: voice, mood, tone, pronouns, banned phrases, Lore vocabulary, branding, and the link conventions that make cross-references work. Page shape and typography live in [`format.md`](format.md). For grammar, punctuation, and mechanics not addressed here, the [authority hierarchy](../README.md#authority) applies. Two Lore-specific conventions carry inline rationale: project-voice `we` (below) and em-dash spacing (in [`format.md`](format.md)).

## Audience

Lore docs target a professional engineering audience — open-source contributors and downstream integrators reading on the public internet. Write at an 8th-grade reading level. Many readers are non-native English speakers; simple sentence structure improves comprehension and translation quality.

## Voice

Use active voice. The subject performs the action.

- Right: `Lore stages the file when you run lore stage.`
- Wrong: `The file is staged when lore stage is run.`

Passive voice is acceptable when the actor is unknown, the actor is irrelevant, or the receiver of the action is the focus.

## Mood

| Doc type | Default mood |
| --- | --- |
| Tutorial | Imperative for steps; indicative for lede and verification |
| How-To | Imperative for steps; indicative for context |
| Reference | Indicative for descriptions; imperative inside example commands |
| Explanation | Indicative |
| Landing (folder index) | Indicative; mostly noun-phrase headings |
| Glossary entry | Indicative, third person |
| Internals | Indicative throughout. Descriptive, not instructional. |
| ADR | Past tense for Context and Problem Statement; present for Consequences and any rules the decision codifies. |
| Code-Standard | Imperative for rule statements; indicative for rationale. |

Never use the subjunctive mood. `Should you wish to publish your branch` becomes `Run lore push to publish your branch`.

Mixing moods within one doc is fine when it serves the reader.

## Tense

Use present tense for descriptive prose: `lore push uploads commits to the remote.`

Past or future tense is acceptable when sequence matters (a condition and its consequence). Restrict yourself to past simple and future simple: `If you choose the wrong remote, lore push will fail.`

## Contractions

Use contractions in body prose throughout Lore docs — every doc type, every folder. The conversational register fits the project's collective maintainer voice and reads well across the doc set.

```markdown
<!-- correct -->
The `--force` flag doesn't bypass branch protection.

<!-- correct -->
You don't need to install anything before starting this tutorial.
```

## Pronouns

Default to **you** when addressing the reader.

- Right: `You can stage files with lore stage.`
- Wrong: `We can stage files with lore stage.`

Never use first-person singular (`I`, `me`, `my`).

First-person plural (`we`, `our`, `us`) speaks for the Lore project — what the project decided, chose, recommends, provides, or did. Use it for project recommendations, design rationale, project history, and statements about what Lore offers.

```markdown
<!-- correct, project recommendation -->
We recommend committing small, focused changes.

<!-- correct, project decision and rationale -->
We chose content-addressed fragments to support offline-first workflows.

<!-- correct, what the project provides -->
We ship a Rust API and a C API as the integration surfaces; integrators choose the one that matches their host language.

<!-- correct, project history -->
In v0.5, we added support for sparse clones.
```

The exception is the **instructional voice** in Tutorials and How-Tos — never write `we` when you mean the reader. Rewrite to imperative or to second person.

```markdown
<!-- wrong, instructional -->
In this tutorial, we will create a new branch.

<!-- right -->
This tutorial creates a new branch.

<!-- also right -->
Create a new branch with lore branch.
```

**Why.** Lore is open-source and contributed to by a multi-organization community. The maintainer voice is collective, not corporate, and the project speaks for itself in its own docs. The instructional ban exists because a reader following a Tutorial is *the* actor — pretending the writer is alongside them obscures who's doing what.

Use singular `they` / `them` / `their` for gender-neutral reference. Active voice often eliminates the need for any pronoun — prefer the rewrite when it's clean.

> [!NOTE]
> `.vale.ini` disables `Lore.We` — Vale can't distinguish project-voice `we` from instructional `we`, so that distinction is reviewer-enforced.

## Word choice

### Lore vocabulary, not Git-isms

Lore is its own version-control system. Don't import Git or Perforce mental models — Lore has its own primitives, and forcing a Git-shaped translation onto them tends to mislead more than it teaches.

When introducing a Lore concept, name it on its own terms and define it inline on first use. When a comparison is genuinely useful — most often, when orienting Git users — use an explicit translation slot ("Unlike Git's `rebase`, Lore offers a different model") rather than a quiet substitution.

The two tables below cover the load-bearing terms only. They aren't an exhaustive catalogue of every Git or Perforce term — they're the ones writers most often get wrong.

#### Core Lore terms

Lore-native primitives. Use as written; define inline on first use.

| Term | Meaning |
| --- | --- |
| `commit` | The act of recording a snapshot, and the resulting record. Same word as Git, distinct mechanics. |
| `revision` | An entry in a branch's history. Lore-distinctive — don't collapse into "commit." |
| `branch` | First-class. Branches in Lore are full citizens, not pointers. |
| `latest` | The local pointer to the most recent revision on a branch. Lore's analog to Git's `HEAD`, but per-branch and not exposed as a roving pointer. |
| `working tree` | The on-disk state of a repository instance. |
| `stage` | The set of changes recorded for the next commit. The verb (`lore stage`) and the noun. |
| `repository instance` | An addressable copy of a repository on a peer. Created by `lore clone`. |
| `clone` | The act (`lore clone`) and the resulting repository instance. Lore-native — not a Git-ism. |
| `fragment` | Lore's content-addressed unit of storage. |
| `sync` | Lore's analog to Git's `pull`. |
| `view filter` | Client-side glob-based path include/exclude, user-defined per instance and changeable in place (`lore clone --view`, `lore sync --view`). Close to a P4 stream view, but per-instance rather than server-side. Closest Git analog: sparse-checkout. |
| `layer` | A repository mounted at a path inside another repository, tracking current and staged revisions with revision-matching metadata. One of two Lore analogs to a Git submodule. |
| `link` | A pinned reference to a path in another repository (`--pin <branch-or-revision>`). The other Lore analog to a Git submodule. |

#### Concepts with no Lore analog

Lore doesn't have these. Don't translate — rewrite around the Lore primitive that does the equivalent job, or leave them out.

| Term | Why |
| --- | --- |
| `shelve`, `unshelve`, `stash` | No analog. Use a `branch` for in-flight work that needs to be set aside. |
| `detached HEAD` | Lore's branch + revision model doesn't enter this state. |

Vale `Lore.GitIsmSubstitutions` and `Lore.PerforceIsmSubstitutions` rewrite Git and Perforce terms that have a clean 1:1 Lore equivalent (`HEAD` → `latest`, `working copy` → `working tree`, `changelist` → `revision`, `git index` → `stage`, `p4 integrate` → `lore branch merge`, others); `Lore.GitIsmFlagged` and `Lore.PerforceIsmFlagged` flag terms that need writer judgment — either no Lore analog (`detached HEAD`, `git stash`, `refspec`, `depot`, `p4 stream`) or multiple Lore analogs depending on intent (`submodule` → `layer` or `link`). The Vale rules cover a broader set than the tables above — those are the load-bearing terms for writers; the rules are exhaustive.

### Difficulty descriptors

Don't claim a step is easy. Difficulty is relative — phrases like `simply add`, `easily replaced`, `Just create a folder`, or `all you have to do is` alienate readers who don't find the task simple. The same words have legitimate non-difficulty senses (`just like`, `just a list of`, `breaks easily`) and those are fine.

If a workflow is genuinely hard, explain *why*: `This workflow requires familiarity with three-way merge resolution.`

Vale `Lore.Difficulty` flags the difficulty-claim register specifically: imperative softeners (`Just <verb>`, `simply <verb>`), passive ease claims (`easily <verb>`, `easily <X>-able`), `all you have to do is`, and bare `obviously`. It doesn't fire on every appearance of `just`/`simply`/`easily`.

### Modifiers

Adjectives and adverbs are seldom needed in technical writing. Reach for stronger nouns and verbs first; cut modifiers that don't carry information. The exception is when a qualifier is load-bearing — `closely related branches` is fine if the relatedness is the actual claim. Not Vale-enforced; reviewers catch this.

### Permission verbs

Don't use `allow`, `let`, `permit`, or `enable` (in the "lets you do X" sense) when describing what a user can do. The software isn't a gatekeeper.

| Wrong | Right |
| --- | --- |
| Lore branches **allow** you to work in parallel. | With Lore branches, **you can** work in parallel. |
| Using `lore stage` **lets** you control which changes are committed. | **Use** `lore stage` to control which changes are committed. |

`enable` (paired with `disable`) is correct only when literally turning a feature on or off: `Enable the pre-commit hook to validate changes automatically.`

Vale `Lore.PermissionVerbs` enforces.

### Settings and options

Use **setting** for persistent values a user configures once — config-file fields (`~/.config/lore/config.toml`), environment variables (`LORE_HOME`), behavioral toggles that stay set. Use **option** for per-invocation choices — CLI flags (`--force`), menu picks, values passed to a single operation. The same UI control can be either; pick the one that matches its lifetime and stay consistent within a doc.

### Other terms to avoid

| Avoid | Use |
| --- | --- |
| `&` | **and**. Acceptable only inside proper feature names. |
| `and/or`, `either/or` | Rewrite. *Wrong:* "you can wear jeans `and/or` sweats." *Right:* "you can wear jeans or sweats." *(`Lore.AndOr`)* |
| `and then`; sentence-starting `then` | Use one or the other, not both. Don't start a sentence with `then` — comma it onto the previous clause, or split. *Wrong:* "Save your work. Then close the editor." *Right:* "Save your work, then close the editor." |
| `backwards`, `towards`, `whilst` | `backward`, `toward`, `while` (American spelling). *(`Lore.AmericanSpelling`)* |
| `boolean` (lowercase prose) | `Boolean` (capitalized). *(`Lore.Boolean`)* |
| `VS`, `vs.`, `VS.` | `vs` (lowercase, no period). *(`Lore.VsAbbrev`)* |
| `native code` | `unmanaged code` *(`Lore.NativeCode`)* |
| `blacklist`, `whitelist` | `denylist`, `allowlist` *(`Lore.BlackWhiteList`)* |
| `drop-down`, `drop down` | `dropdown` *(`Lore.Dropdown`)* |
| `master`, `slave` *(paired)* | Pick the closest fit and stay consistent: `primary/cluster`, `parent/child`, `primary/secondary`, `main/replica`, `initiator/target`, `requester/responder`, `controller/device`, `host/worker`, `host/proxy`, `leader/follower`. |

`affect` (verb, to impact) versus `effect` (noun, the result). Avoid effect-as-verb.

### No Latin abbreviations

Spell them out.

| Avoid | Use |
| --- | --- |
| `cf.` | compare |
| `e.g.` | for example |
| `etc.` | and the rest |
| `i.e.` | that's |
| `n.b.` | Note |

Vale `Lore.LatinAbbrev` enforces.

### No idioms or slang

Idioms translate badly. When in doubt, ask whether a non-native English reader would parse the phrase literally — if literal parsing changes the meaning, rewrite.

Examples to avoid: *get your feet wet*, *take it for a spin*, *deep dive*, *kill two birds with one stone*, *a piece of cake*.

### Lore-technical exception: `kill`, `die`

`kill` and `die` are acceptable in Lore docs in their established technical meanings: `kill -9`, signal handling, "the process dies on SIGHUP." Avoid them only when describing a UI action, where `eliminate`, `remove`, or `delete` is clearer.

## Branding

### Lore product naming

Lore is a product made up of three named parts:

- **Lore CLI** — the command-line client. Reach for this name when the part needs to be distinguished from Lore Server or the Lore library.
- **Lore Server** — the server you push to and clone from.
- **Lore library** — the Rust crates an embedding application links against.

The canonical rules:

- **Lore** (capital `L`, unqualified) when referring to the product as a whole, the project, or the open-source project, and the meaning is clear from context. Example: `Lore is a version control system.`
- **`lore`** (lowercase, in a code span) when referring to the CLI **binary** or a specific command. Example: `Run lore stage to add files to the next commit.`
- **Lore CLI** / **Lore Server** / **Lore library** when you need to disambiguate which part of the product you mean. Example: `The Lore CLI is bundled with every release; the Lore Server is shipped separately.` Don't capitalize `library`.

Don't write *the Lore tool*, *the Lore software*, *the Lore application*, *the Lore product*, or *the Lore system* — these are generic qualifiers and `Lore` already covers them. Use one of the three named parts above when you need to be specific, otherwise just write `Lore`. Vale `Lore.ProductNaming` enforces the generic-qualifier ban.

### Category naming

Lore is a **version control system**. Use "version control" — not "revision control" — as the category term wherever it appears: prose, headings, and titles. This is separate from the Lore primitive `revision` (an entry in a branch's history; see [Core Lore terms](#core-lore-terms)), which keeps its name. Only the two-word category phrase is governed here — never rewrite a standalone `revision`.

When the project needs to be named unambiguously, pair the name with the category: **Lore version control**. Use it where a reader first meets the project on a page; plain `Lore` is fine once context is set. Don't repeat the full phrase past its first natural use.

> [!NOTE]
> **Why.** "Version control" is the standard name for the category and how every peer tool — Git, Mercurial, Perforce — describes itself; "revision control" is a dated synonym. And *lore* is a common English word, so pairing the name with the category keeps the open-source project recognizable.

### Third-party brands

Spell third-party brand names exactly as they appear on the brand's official site, including capitalization. When Merriam-Webster and the brand's site disagree, follow the brand.

- `NVIDIA`, not `Nvidia`.
- `RocksDB`, not `RocksDb`.

Don't name third-party brands or products in Lore docs unless the page is specifically about integrating that product with Lore. This keeps content evergreen — third-party products change names, get acquired, and disappear.

Don't use `™`, `®`, or `©` glyphs in prose unless contractually required.

## Cross-references

Use **see** when cross-referencing. **Refer to** is verbose. Provide the link with the name of what you're linking to — no filler.

- Right: `See [Branching basics](../branching/basics.md).`
- Wrong: `See the page about branching basics here: [Branching basics](../branching/basics.md).`

Don't paste raw URLs in body text. Wrap with descriptive link text. Exception: glossary entry definitions use bare URLs so glossary tooling can autolink them — see [`operational/glossary-conventions.md` § External links inside definitions](../operational/glossary-conventions.md#external-links-inside-definitions).

### Link text

Link text must describe the destination. Banned link text: `click here`, `here`, `this`, `read more`, `link`. Vale `Lore.LinkText` enforces.

- Bad: `For more on branching, [click here](docs/branching.md).`
- Good: `Read the [Lore branching guide](docs/branching.md) for the full walkthrough.`

### Third-party documentation links

Readers must understand that the link takes them away from the Lore docs and where it goes.

- **Quick mention:** `Lore's storage layer uses [RocksDB](https://rocksdb.org) (see the RocksDB documentation).`
- **Substantive reference:** `For details about the underlying compaction strategy, see [Compaction](https://rocksdb.org/docs/compaction) in the RocksDB documentation.`
- **Site-as-a-whole:** `The RocksDB documentation has good examples of tuning compaction.`
- **Specific section:** italicize the heading name. `For more, see the *Compaction Triggers* section of the RocksDB documentation.`

## Action verbs

When describing user actions on the rendered docs site or in a generic browser interaction:

| Term | Use |
| --- | --- |
| click | Click a button. Don't write `click on` — left-button click is implied. *(`Lore.ClickOn`)* |
| right-click, double-click | Same rule. Don't write `right-click on` or `double-click on`. *(`Lore.ClickOn`)* |
| select | Select from a list or menu. |
| drag | Preferred over `drag and drop`. Drop is implied. |
| press | Press a key. `Press the Tab key`, not `press Tab` — distinguishes from UI tabs. |
| open | Open a screen, panel, or menu. |
| enable, disable | Pair them. Reserved for turning a feature on or off. |
| expand, collapse | Pair them. Used for nested-content UI areas. |
| appears, will appear | Use when something shows up on screen. Don't use `displays`. |

Lore has no graphical client application. Use these terms when describing the rendered docs site, terminal interactions, or generic browser usage.

## Describing a limited feature

When a feature doesn't yet support a use case, write `you can only X`. Don't write `not yet available` or `coming soon` — `not yet` implies a roadmap commitment.

- Right: `lore branch operates on local branches only.`
- Wrong: `Remote branch operations aren't yet available in lore branch.`
