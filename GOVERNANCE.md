# Governance

Lore is an open source project sponsored by [Epic Games](https://epicgames.com). This document describes how the project is governed: who makes decisions, how they are made, and how contributors can grow into leadership roles.

## Roles

### Contributor

Anyone who has had a pull request merged. Contributors participate in discussions, propose changes, and review pull requests. No formal nomination is required.

### Maintainer

A trusted contributor with merge authority. Maintainers take responsibility for the quality and consistency of what lands in the codebase and can approve and merge pull requests. A maintainer's remit may span the whole project or focus on a specific area — such as documentation, the desktop client, CI, or the client SDKs. The current Maintainers, and any area focus, are listed in [MAINTAINERS.md](MAINTAINERS.md).

**Path to Maintainer:** Consistent contributions over at least 3 months — bug fixes, features, or documentation — and working knowledge of the relevant parts of the codebase. Nominated by a Core Maintainer; existing Maintainers have 7 days to object.

### Core Maintainer

Stewards of the project responsible for technical direction, security, and releases. Core Maintainers make final calls on technical matters when consensus fails. The current Core Maintainers are listed in [MAINTAINERS.md](MAINTAINERS.md).

**Path to Core Maintainer:** Active Maintainer with contributions spanning multiple areas of the project. Requires unanimous approval from existing Core Maintainers.

### Steering Group

The Steering Group sets the project's strategic and product direction: roadmap priorities, scope, and ecosystem and community matters. Membership is not limited to people who contribute code directly and may include Maintainers alongside product and engineering leaders. The current members are listed in [MAINTAINERS.md](MAINTAINERS.md).

**Path to the Steering Group:** Consistent collaboration over at least 6 months — community engagement, user feedback, feature requests, etc. — and a deep understanding of Lore's users and Lore itself. Nominated by a current Steering Group member; existing members have 7 days to object.

---

## How contributions are accepted

1. **Open an issue** — describe what you want to build or fix. For non-trivial changes, wait for a maintainer to weigh in before investing significant effort.
2. **Discuss** — the issue receives a minimum 48-hour feedback window. For architectural questions, use [Discord](https://discord.gg/QYbNFVFv) or the GitHub Issue itself.
3. **Open a pull request** — follow the guidelines in [CONTRIBUTING.md](CONTRIBUTING.md). Use the PR template.
4. **Review** — two approvals from Maintainers are required.
5. **Merge** — a Maintainer merges the PR.

An objection blocks a PR. If it cannot be resolved through discussion, it is escalated to the Core Maintainers for a final decision.

## Lore Enhancement Proposals

Significant changes to the wire protocol, on-disk format, or public APIs require a [Lore Enhancement Proposal (LEP)](docs/proposals/README.md) before implementation begins. LEPs go through a minimum 2-week discussion period before a decision is made. An accepted LEP is binding; the implementation must match the proposal.

## Decision-making

For most decisions, consensus in the GitHub Issue or PR discussion is sufficient. When consensus fails, Core Maintainers decide technical matters: what gets merged, releases, architecture, and LEPs. The Steering Group decides strategic matters: roadmap, scope, and community direction. Changes to the project's fundamental scope are the exception — they follow the major-decision process below. All decisions are made in public — in issues, PRs, or LEP discussions.

Major decisions — license changes, changes to this document, or changes to the project's fundamental scope — are announced publicly with at least 2 weeks for community input before any change is made.

## Role lifecycle

### Emeritus status

Maintainers inactive for 12 or more consecutive months may be moved to Emeritus status. Emeritus contributors retain advisory standing but hold no merge authority. They may request reinstatement at any time; a Core Maintainer sponsor and no objections from existing Maintainers restores active status.

### Removal

A Maintainer may be removed for a sustained Code of Conduct violation. Removal requires Core Maintainer consensus and is a last resort — the enforcement ladder in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) applies first.

### Core Maintainers and the Steering Group

A Core Maintainer or Steering Group member may step down at any time by notifying the others in that body. Prolonged inactivity — the same 12-month guideline as above — or removal for cause is decided by the consensus of the remaining members of that body, with the Code of Conduct enforcement ladder applying first in any conduct case.

## Epic Games' role

Epic sponsors Lore's development and provides infrastructure. Epic-authored commits go through the same review process as any community contribution — no one merges their own code. Lore is MIT-licensed and Epic does not intend to change that.

## Security

See [SECURITY.md](SECURITY.md). Security issues are handled through Epic's existing security program — do not open public GitHub Issues for vulnerabilities.

## Code of conduct

All community spaces are governed by the [Code of Conduct](CODE_OF_CONDUCT.md). Report violations to [lore-moderation@epicgames.com](mailto:lore-moderation@epicgames.com).

If an alleged violation involves a Core Maintainer, the remaining Core Maintainers handle the investigation without that person's participation.

## Amending this document

Changes to this document are announced publicly with at least 2 weeks for community input before any change takes effect.
