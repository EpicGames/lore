# Roadmap

This page shows where Lore is headed at a high level — the big rocks we're working toward, grouped by rough timeline. For the fine-grained, always-current status of any single feature, the source of truth are the [GitHub Issues](https://github.com/EpicGames/lore/issues) and [Lore Enhancement Proposals](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md#lore-enhancement-proposals) that track it.

The thread running through every theme is the path to a **1.0 stable release**. Lore ships today as a pre-stable 0.x: the formats are built to last — content you commit now stays readable by every future release — but APIs and protocols can still change before 1.0, when strict backward compatibility takes over. The work below is what we believe gets Lore to a 1.0 that studios can adopt with confidence.

## At a glance

| Theme | Timeline | Status |
| --- | --- | --- |
| [**Lore OSS and UEFN compatibility**](#lore-oss-and-uefn-compatibility) | 2026 | In progress |
| [**VS Code plugin**](#vs-code-plugin) | 2026 | In progress |
| [**Scalable file locking**](#scalable-file-locking) | 2026 | In progress |
| [**Virtual file system (VFS)**](#virtual-file-system) | 2026 | In progress |
| [**Links and layers**](#links-and-layers) | 2026 | In progress |
| [**Desktop client**](#desktop-client) | 2027 | In progress |
| [**Unreal Editor plugin**](#unreal-editor-plugin) | 2027 | In progress |
| [**Web client**](#web-client) | 2027 | Committed |
| [**Edge instances and advanced server topologies**](#edge-instances-and-advanced-server-topologies) | 2027 | Exploring |
| [**Forks and isolated partitions**](#forks-and-isolated-partitions) | Later | Exploring |

**Status legend**:

- **In progress** — actively underway, with the foundations already in place
- **Committed** — we've committed to delivering it, but the work hasn't started yet
- **Exploring** — we know we want it and are working out the shape

> [!NOTE]
> This roadmap is **directional, not a guarantee**. Timelines are targets, not commitments, and the order reflects today's thinking. As a 0.x project shaped in the open, priorities move in response to what the community builds, reports, and asks for — so expect this page to evolve. Want to influence it? See [Help shape the roadmap](#help-shape-the-roadmap).

## 2026 — foundations

The near-term focus is putting the foundations that team-wide, large-scale deployments depend on in place: locking that scales, workflows that stay fast on the largest repositories, finishing the multi-repository composition the data model already supports, and bringing Lore into the editor developers already work in.

### Lore OSS and UEFN compatibility

**Timeline:** 2026 · **Status:** In progress

**Converge the OSS and UEFN implementations of Lore so open-source clients, libraries, and SDKs can talk to UEFN's hosted Lore implementation directly.** Lore is the built-in version control system for UEFN (Unreal Editor for Fortnite), but today's open-source tooling can't talk to it: Lore uses the open-source Zstandard compression format, but UEFN projects use Oodle, which isn't compatible. We're actively moving UEFN onto Zstandard to eliminate the gap between the two.

### VS Code plugin

**Timeline:** 2026 · **Status:** In progress

**Bring a graphical Lore interface into Visual Studio Code, where everyday operations are a click away.** Everything in Lore is reachable through the CLI and API today; this plugin surfaces those operations visually, in an editor many developers already use — making version control discoverable instead of something you have to memorize. We intend to release both the plugin and its source code as a complementary repository as part of the overall Lore project.

### Scalable file locking

**Timeline:** 2026 · **Status:** In progress

**Scale locking so that it can enforce single-editor access across millions of files and thousands of concurrent users.** Lore has basic locking today — users can lock a non-mergeable asset to signal that it's being edited — but the current implementation informs rather than enforces, and lock state is queried globally across the repository, which doesn't scale. The next iteration focuses on enforcement and cross-branch lock scalability at that size, so lock state stays correct and cheap to query no matter how large the repository or team grows. Locking is the recommended workflow for binary assets where two people should never be editing at once, so making it scale is foundational for large art teams.

### Virtual file system

**Timeline:** 2026 · **Status:** In progress

**Implement a virtual file system (VFS) so you can be productive in a multi-terabyte project moments after cloning — and never store it more than once.** Lore works sparsely today — a clone brings down only the part of the project you ask for — but you choose that part up front, and every branch or workstream you keep locally is another full copy on disk. By adding a virtual file system, Lore can load files lazily as you open them and serve them from Lore's existing [shared store](glossary.md#shared-store), extending Lore's fragment-level [deduplication](glossary.md#deduplication) and ensuring that content only lives on disk once. The larger a project grows and the more branches a team runs at once, the larger the cost of up-front clones, incremental syncs, and on-disk storage — so a VFS is foundational to keeping growing projects workable.

### Links and layers

**Timeline:** 2026 · **Status:** In progress

**Unlock composition of multiple repositories into a single working tree, with access scoped to each link.** Lore's data model already expresses multi-repository composition through [links](glossary.md#link) and [layers](glossary.md#layer): a link is a pinned reference to a subtree of another repository, recorded in the parent's revision so it travels with every clone, while a layer overlays one repository's content onto another at materialization time. Each linked repository is its own [partition](glossary.md#partition) with its own access control, which is how Lore expresses per-directory access policy — but today that capability lives in the data model rather than in a workflow you can reach for. We're finishing and exposing it as a first-class operation, so teams can easily assemble shared libraries and multiple repositories into one working tree while still ensuring that each contributor only sees the partitions they're entitled to.

## 2027 — scale and collaboration

With the foundations in place, the focus shifts to scale and collaboration: graphical clients — desktop, web, and a plugin inside the Unreal Editor — that give teams a shared way to work, and optimizing replication so even the most spread-out teams keep working against a fast, nearby server.

### Desktop client

**Timeline:** 2027 · **Status:** In progress

**Open-source the desktop client so the community can build on its full graphical experience, not just download it.** An early desktop client already exists as a binary download, but it isn't open source yet — it depends on some proprietary components, including Epic's internal design system. We're working to make all of it available in the open so that the client can ship as source alongside the rest of Lore. Lore is an open project, so it is important that the desktop client — which will be one of the main ways many people will interact with Lore — is also fully open so that the community is free to review, extend, and shape it.

### Unreal Editor plugin

**Timeline:** 2027 · **Status:** In progress

**Surface a visual version-control workflow inside the Unreal Editor, the interface you already work in.** A Lore plugin brings everyday Lore operations directly into the tool artists and developers use to build with Unreal Engine, so versioning your work is a native part of the editor rather than a separate command-line step — and artists who never touch a terminal can manage it themselves. It builds on the Lore plugin that already ships with UEFN, and rather than living as a separate Lore project, it will be included in the native Unreal Engine codebase and delivered as part of the engine itself.

### Web client and code review tools

**Timeline:** 2027 · **Status:** Committed

**Open-source the web client so teams get a shared, browser-based home for code review and repository management.** Provide a home where changes get reviewed and discussed before they land, and where teams manage their repositories day to day — all in the browser. Unlike a desktop binary, a web client is a little more complex to package in a form that any team can readily stand up and self-host. We're working through exactly that, so the web client can ship as source alongside the rest of Lore. Similar to the desktop client, it's important that the web client is also fully open, so the community is free to review, extend, and shape it.

### Edge instances and advanced server topologies

**Timeline:** 2027 · **Status:** Exploring

**Iterate and optimize Lore server replication to support even the most extreme use cases.** Lore already supports distributed teams through server replication across clustered servers and regions. This work is about the extreme cases where there's room to optimize how Lore replicates and caches so it stays fast no matter how far a project reaches. The more locations and regions a studio spans, the more its everyday speed depends on replication holding up under that load — so optimizing it is foundational for the largest, most distributed teams.

## Later — exploring

These are directions we're confident about but still shaping. Timelines firm up as the foundations above land and as the data model proves out in real use.

### Forks and isolated partitions

**Timeline:** Later · **Status:** Exploring

**Add the ability to fork projects so that teams can experiment and take projects in unique directions, as an independent, access-controlled copy whose changes can always merge back into its source.** The data model already describes a [fork](glossary.md#fork) as a separate [partition](glossary.md#partition) with its own access control — it shares a source repository's initial content but evolves independently, filled in lazily through copy-on-write. But that capability lives in the data model rather than a workflow you can reach for. We intend to finish and expose forks as a first-class operation, so that separate teams can push shared projects forward without sharing write access — each working in their own access-controlled copy, each able to merge work back to the shared source.

## Help shape the roadmap

Lore is built in the open, and this roadmap reflects community input as much as our own plans. There are a few ways to weigh in:

- **Have an idea or a need?** Post in [`#feature-requests` on Discord](https://discord.gg/E4SFJKRPbg) or open a [GitHub Issue](https://github.com/EpicGames/lore/issues). Real-world use cases are what move items up this page.
- **Want to build one of these?** Many of these themes are great places to contribute. Read [CONTRIBUTING.md](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md) and look for the [`good-first-issue`](https://github.com/EpicGames/lore/labels/good-first-issue) label to get started.
- **Proposing a significant change?** Changes to the wire protocol, on-disk format, or public APIs go through a [Lore Enhancement Proposal](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md#lore-enhancement-proposals) — the place where the biggest roadmap items get designed in public.

Governance of Lore is evolving toward a technical steering group drawn from both internal and external contributors, operating through public roadmaps, RFCs, and open meetings. This page is part of that commitment to deciding Lore's direction in the open.
