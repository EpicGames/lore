# Governance

Lore is an open source project sponsored by [Epic Games](https://epicgames.com). This document describes how the project is governed: who is responsible for what, and how decisions are made. How contributions are made and accepted is in [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Roles

### Contributor

Anyone who has had a pull request merged. Contributors participate in discussions, propose changes, and review pull requests. No formal nomination is required.

### Maintainer

A trusted contributor with merge authority. Maintainers take responsibility for the quality and consistency of what lands in the codebase and can approve and merge pull requests. A maintainer's remit may span the whole project or focus on a specific area — such as documentation, the desktop client, CI, or the client SDKs. The current Maintainers, and any area focus, are listed in [MAINTAINERS.md](MAINTAINERS.md).

### Core Maintainer

Stewards of the project responsible for technical direction, security, and releases. Core Maintainers make final calls on technical matters when consensus fails. The current Core Maintainers are listed in [MAINTAINERS.md](MAINTAINERS.md).

### Steering Group

The Steering Group sets the project's strategic and product direction: roadmap priorities, scope, and ecosystem and community matters. Membership is not limited to people who contribute code directly and may include Maintainers alongside product and engineering leaders. The current members are listed in [MAINTAINERS.md](MAINTAINERS.md).

---

## Decision-making

For most decisions, consensus in the GitHub Issue or PR discussion is sufficient. When consensus fails, Core Maintainers decide technical matters: what gets merged, releases, architecture, and LEPs. The Steering Group decides strategic matters: roadmap, scope, and community direction. Changes to the project's fundamental scope are the exception — they follow the major-decision process below. All decisions are made in public — in issues, PRs, or LEP discussions.

Major decisions — license changes or changes to the project's fundamental scope — are announced publicly for community input before any change is made.

## Role lifecycle

### Emeritus status

Maintainers inactive for 12 or more consecutive months may be moved to Emeritus status. Emeritus contributors retain advisory standing but hold no merge authority. They may request reinstatement at any time; a Core Maintainer sponsor and no objections from existing Maintainers restores active status.

### Removal

A Maintainer may be removed for a sustained Code of Conduct violation. Removal requires Core Maintainer consensus and is a last resort — the enforcement ladder in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) applies first.

### Core Maintainers and the Steering Group

A Core Maintainer or Steering Group member may step down at any time by notifying the others in that body. Prolonged inactivity — the same 12-month guideline as above — or removal for cause is decided by the consensus of the remaining members of that body, with the Code of Conduct enforcement ladder applying first in any conduct case.

## Epic Games' role

Epic sponsors Lore's development and provides infrastructure. Lore is MIT-licensed and Epic does not intend to change that.

## Security

See [SECURITY.md](SECURITY.md). Security issues are handled through Epic's existing security program — do not open public GitHub Issues for vulnerabilities.

## Code of conduct

All community spaces are governed by the [Code of Conduct](CODE_OF_CONDUCT.md). Report violations to [lore-moderation@epicgames.com](mailto:lore-moderation@epicgames.com).

If an alleged violation involves a Core Maintainer, the remaining Core Maintainers handle the investigation without that person's participation.
