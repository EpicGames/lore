# Frequently asked questions

## What is Lore?

Lore is a next-generation open source version control system maintained by Epic Games. Designed for unprecedented scalability — both in terms of data size and the size and distribution of teams and repositories — it’s especially optimized for projects that combine code with large binary assets, such as those found in the games and entertainment industries. Easy to set up, scale, and maintain, Lore provides the foundations for teams to build fast, intuitive, and practical collaborative workflows for developers and artists alike.

## How is Lore licensed?

Lore is fully open source and released under the MIT license. You can use it for any project, modify it, distribute it, and build on top of it without restriction or license fees.

## Why open source?

We believe a truly open ecosystem won’t be built by any one company, but collectively and collaboratively using open standards. A version control system shaped behind closed doors would fall short of that — the formats, protocols, and design decisions that underpin how teams collaborate on creative work should be open to scrutiny, challenge, and improvement by the people who use them.

Open-sourcing Lore means the community can extend it, build on top of it, and help shape it into something more powerful than any single team could produce alone. It also means no lock-in: an MIT license and a fully public specification ensure that Lore’s future belongs to its users and contributors, not to any one company’s roadmap.

## How does Lore differ from other version control solutions?

No existing system was designed for the combination of constraints that large game and entertainment projects require: arbitrary content types, multi-axis scale, multi-tenant safety, and a fully open specification and license.

Git’s content-addressed revision graph is excellent, but it treats binary files as second-class citizens — large files require bolted-on LFS rather than first-class chunked storage, sparse checkouts have sharp edges in offline use, and there is no native multi-tenant isolation.

Centralized systems designed for large binary content handle those assets well but typically require server round trips for everyday operations, use proprietary wire protocols that foreclose third-party implementation, and offer limited deduplication at the binary level.

Lore is designed to combine what works in each: a centralized server-of-record for durability, access control, and conflict resolution; content-addressed storage with fragment-level deduplication that’s as effective on a multi-gigabyte binary as on a kilobyte of text; sparse, lazy working trees that materialize only what you need; free branching; and a fully open, publicly versioned specification and MIT license. Normal editing operations — staging, committing, branching, diffing — never require a network round trip.

## What platforms does Lore support, and where can I find the SDKs?

Lore runs on Windows, macOS (ARM64), and Linux (x86-64, ARM64). Pre-built library, CLI, and server binaries are available for all three platforms, and a Docker container image is provided for server deployment. The library interface is directly usable in C/C++ and Rust. Language bindings for JavaScript, C#, Python, and Go are available through their ecosystems’ standard package tooling. All source code is hosted on GitHub.

## How do I get started?

You can be up and running in minutes. Download the pre-built binary for your platform, and launch the server in local mode — no authentication or cloud configuration required. The [quickstart guide](tutorials/quickstart.md) walks you through creating a repository, submitting content, syncing, and branching in under 10 minutes. When you’re ready to scale, add authentication and connect to your cloud provider of choice.

## Does Lore support file locking?

Lore has basic locking today: users can place locks on files to signal to collaborators that a non-mergeable asset is being edited, which helps prevent accidental conflicts on binary files.

What isn’t yet in place is enforcement — the current implementation informs rather than blocks, and lock state is queried globally across the repository, which doesn’t scale to very large projects.

The next iteration of locking, currently in design, will focus on cross-branch lock scalability in repositories with millions of files and thousands of concurrent users. We’ll share updates on progress through the [public roadmap](roadmap.md) and Discord.

## What infrastructure do I need to run a Lore server?

For local use, none — Lore’s zero-config local mode runs as a single server executable with no external dependencies, no certificates, and no cloud account required.

For team-wide deployments, Lore’s storage backends are designed to be replaceable: the reference implementations ship with AWS S3 for durable immutable storage and DynamoDB for the mutable key-value store (branch pointers and metadata), but the storage and transport layers sit behind documented interfaces that third-party backends can implement. The server can be deployed via Docker or a pre-built binary and scaled horizontally with edge cache and read replica tiers in front of durable storage. The initial release supports a two-level server hierarchy; broader multi-level topologies are [on the roadmap](roadmap.md).

## How does Lore handle merge conflicts?

Lore is binary-first by design: all content flows through the same storage primitives as opaque bytes, with text-aware features layered on top rather than assumed at the core.

For text files, the version control layer ships a standard three-way merge implementation that handles the common case of two contributors editing different parts of the same file.

For binary assets — where automatic merging is rarely meaningful — conflicts surface as explicit divergence: the system detects that two branches have modified the same file and surfaces the conflict for the user to resolve by choosing which version to carry forward. File locking is the recommended workflow for binary assets where two people should never be editing simultaneously in the first place. More advanced capabilities — such as partial binary merges exploiting Lore’s fragment-level storage, and pluggable custom resolvers by content type — are part of the [longer-term roadmap](roadmap.md).

## How do I report a security vulnerability?

Please don’t open a public GitHub issue for security vulnerabilities. Instead, follow Epic Games’ responsible disclosure process — details are in the [`SECURITY.md`](https://github.com/EpicGames/lore/blob/main/SECURITY.md) file in the repository. The team maintains a defined internal response process for public repository security incidents and will acknowledge reports promptly.

## Is Lore production-ready?

Lore is launching as a pre-stable 0.x release, meaning APIs and protocols may still evolve before we reach a 1.0 stable release — at which point strict backward compatibility will apply.

Even in 0.x, data written into Lore is designed to remain readable by every future release. Content committed to a repository now won’t age out.

We know that version control is a domain where trust is earned over time, and we’re not expecting studios to replace their existing tools overnight with what we have today. The goal right now is for developers and technical leaders to explore Lore, prototype workflows on top of it, and help shape where it goes next.

## Does Lore have a desktop client?

Yes. Taking advantage of Lore’s extensibility, we’ve built an early desktop client, which is available now as a binary download on our [releases page](https://github.com/EpicGames/lore/releases). It isn’t open source yet — it currently depends on some proprietary components, including Epic’s internal design system — but we’re committed to open-sourcing it in the future. In the meantime, everything you can do through the desktop client and more is fully accessible via the CLI and API.

*Please note:* Lore-backed UEFN projects can't yet be opened with Lore Desktop. Lore uses open source Zstandard compression, but UEFN projects previously used Oodle and aren't compatible. Support for UEFN projects will be available in an upcoming release once they have been migrated to use Zstandard.

## Where is Lore already in use at Epic?

Lore, formerly called Unreal Revision Control, is the built-in version control system for UEFN (Unreal Editor for Fortnite), where creators have been using it to version their islands. It’s also seeing progressive adoption by internal Epic teams, and is being implemented as the backing store for UEFN’s cook pipeline, where it replaces traditional intermediary storage layers — eliminating redundant file transfers and significantly reducing the time between publishing changes and those changes being playable.

## How can I contribute?

All contributions are welcome via GitHub. Lore uses DCO (Developer Certificate of Origin) rather than a CLA — this avoids the legal overhead that prevents many contributors from participating. To get started, read the [contribution guidelines](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md) in the repository, pick up an open issue, and submit a pull request. You can also [join the conversation on Discord](https://discord.gg/E4SFJKRPbg), where the maintainers are active.

## Will Lore always be fully open source?

Yes. Our principle is that contributors must have equal access to Lore’s features as Epic’s own engineers. Any core Lore functionality that exists in Lore will be available in the open — no hidden, restricted, or paywalled features. We’re also committed to open-sourcing more of the tools we’re building on top of Lore down the line, including a desktop and web client.

## What’s on the roadmap?

Planned additions include expanded large-repository workflows (VFS, Windows Service), OAuth integration, scalable locking, edge instances and advanced server topologies, client and server-side hooks, a VS Code plugin, and an open source desktop and web client. The [roadmap](roadmap.md) lays out these big-rock themes by time horizon, with the reasoning behind each one.

## Who maintains Lore?

At initial release, Lore is maintained by a dedicated team at Epic Games, but we intend for governance to evolve toward a diverse technical steering group drawn from both internal and external contributors, operating through [public roadmaps](roadmap.md), RFCs, and open meetings.
