---
name: write-lep
description: How to draft a Lore Enhancement Proposal in docs/proposals/ — naming, drafting against the template, applying the rules in the README, and finishing the file.
argument-hint: [title]
---

# Write LEP

Mechanical authoring of a new LEP. The rules — per-section and cross-cutting — live in `docs/proposals/README.md`. The template at `docs/proposals/lep-template.md` is just the section skeleton. This guide is the procedure for applying both.

## Before you start

Read `docs/proposals/README.md` ("How to write an LEP"). It is the canonical source for what each section requires and for the cross-cutting rules (active voice, public citations, no fabrication).

Read `docs/proposals/lep-template.md` to confirm the section structure and frontmatter shape.

Find out what the user has by way of prior research: notes, related reviews, design conversations, related tickets, pasted content, file paths, glob patterns, public links. Any form is fine. If they have nothing, proceed without.

If the user did not pass a title as an argument, ask for a short descriptive one.

## Naming the file

Filenames follow `YYYY-MM-DD-<slug>.md`, where `<slug>` is the kebab-case form of the title (lowercase, hyphens, no punctuation, max ~6 words). Today's date comes from `date +%Y-%m-%d`. If a file with the chosen name already exists in `docs/proposals/`, append `-2`, `-3`, etc. until you reach a free name.

## Drafting

Before drafting, check the scope against the README's *One decision per proposal*. When the work has parts that can be accepted on their own, write a proposal for each. A part that changes an accepted LEP is an amendment to it, as the README's *Lifecycle* describes. Tell the user the split before you draft.

Start from the template's structure — same sections in the same order. For each section, apply the rules in `docs/proposals/README.md` and replace the template's placeholder text with your content.

When research and user input genuinely don't support a claim, mark `{TODO: <what's missing>}` in place rather than inventing detail.

Do not editorialize about the proposal's quality. Apply the rules; gaps become `{TODO}`s, not opinions.

## Finishing

Set the frontmatter: `lep` is the filename without `.md`, `status` is `Draft`, `created` and `updated` are today's date. Write the file to `docs/proposals/<filename>`.

Tell the user to run `/review-lep docs/proposals/<filename>` before sharing the LEP. Staging, committing, and opening it for review are the author's calls — this guide stops at writing the file.
