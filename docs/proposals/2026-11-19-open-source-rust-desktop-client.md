---
lep: 2026-11-19-open-source-rust-desktop-client
title: Open source Rust desktop client
authors:
  - {TODO: author name}
status: Draft
created: 2026-11-19
updated: 2026-11-19
---

# Open source Rust desktop client

## Summary

Lore has a command-line client (`lore`), a C API (`lore-capi`) and a Rust library (`lore`), but its only
graphical client ships as a closed binary. This proposal adds `lore-desktop`, a new workspace crate that builds
a graphical desktop application in Rust. It calls the `lore` library crate directly, the same library the CLI
and the C API wrap, so it adds no new path into the repository. The first release covers the daily loop of a
repository instance: see the working tree's status, stage and commit, browse history and diffs, switch and merge
branches, sync with the remote, and take and release locks.

## Motivation

- **The graphical client is not open.** The [FAQ](../faq.md#does-lore-have-a-desktop-client) states that the
  existing desktop client "isn't open source yet" because "it currently depends on some proprietary components,
  including Epic's internal design system". The [roadmap](../roadmap.md#desktop-client) lists an open desktop
  client for 2027. Until one exists, nobody outside Epic can review, fix or extend the client the roadmap
  expects to be "one of the main ways many people will interact with Lore".
- **Lore's users are not all CLI users.** Lore targets game teams with large binary assets
  ([system design §2.1](../explanation/system-design.md#21-the-workloads-lore-was-built-to-handle)). Artists and
  designers on those teams work with locks, large file lists and binary diffs, which a terminal shows poorly.
- **The API is ready for a second application.** [System design §6.1](../explanation/system-design.md#61-the-api-is-the-primary-artifact)
  makes the API the primary artifact and names the CLI as only "one application that uses the API". Every
  version control operation is already a function in the `lore` crate (for example `lore::repository::status`
  in `lore/src/repository.rs`) that reports its results as events through a `LoreEventCallback`. A graphical
  client is the application that exercises that design beyond the CLI.

## Goals / Non-Goals

### Goals

- **G1.** An open source desktop application, built from this repository with `cargo`, that runs on Windows,
  macOS and Linux.
- **G2.** It reaches every repository through the public `lore` library API only, with the same semantics the
  CLI gets.
- **G3.** It covers the daily loop of a repository instance: status, stage and unstage, commit, history, diff,
  branch create/switch/merge with conflict resolution, sync, push, and locks.
- **G4.** It stays responsive on repositories with very large working trees and histories.
- **G5.** It shows changes other users make, such as pushes and locks, without the user refreshing.
- **G6.** It depends on no proprietary component.

### Non-Goals

- **Reproducing the existing closed binary.** This proposal does not aim for parity with Epic's client, its
  design system or its look.
- **Server administration.** Repository creation on a remote, storage maintenance, partitions and server
  configuration stay in the CLI.
- **A web client.** The [roadmap](../roadmap.md#web-client) tracks the web client separately.
- **An Unreal Editor plugin or IDE integration.** Those are separate applications on the same API.
- **New version control behaviour.** Where the client needs an operation the library lacks, that operation
  gets its own LEP.

## Proposed Design

### A new workspace crate on the library API (G1, G2, G6)

`lore-desktop` is a binary crate in the Cargo workspace. It depends on the `lore` library crate, as
`lore-client` does, and on no internal crate behind it (`lore-revision`, `lore-storage`). Every repository
operation is a call into `lore` with `LoreGlobalArgs`, the call's arguments struct, and a `LoreEventCallback`.
The client therefore inherits the library's behaviour for authentication, the service process, offline mode
and links, and never diverges from the CLI.

All dependencies are open source and pass the repository's `cargo deny` licence policy (`deny.toml`). The
client ships its own small set of widgets and theme rather than a design system.

### Toolkit

The client is written with [egui](https://github.com/emilk/egui) and eframe, a pure-Rust GUI toolkit,
so the whole application builds with the Rust toolchain the workspace already uses and needs no JavaScript
runtime or webview. [ADR-00042](../developing/decisions/00042-desktop-client-toolkit-egui.md) records the
choice.

### Operations off the UI thread (G4)

The UI thread never awaits a library call. Each operation runs on the library's runtime, and its events are
forwarded to the UI as messages that update the view state. The guarantees:

- A long operation (sync, scan, merge) never blocks input or redraws; the UI shows its progress from the
  library's progress events and offers cancel where the library supports it.
- At most one mutating operation runs on an instance at a time from the client; read-only operations may run
  beside it, as [ADR-00026](../developing/decisions/00026-concurrent-read-only-instance-operations.md) allows.
- A failed operation leaves the view as it was before the operation and shows the library's error, with its
  detail.

### Views (G3)

| View | Library surface it uses |
| --- | --- |
| Changes: status, stage, unstage, revert, commit | `lore::repository`, `lore::file`, `lore::revision` |
| History: revision graph and revision details | `lore::revision_graph`, `lore::log` |
| Diff: text side-by-side, binary as metadata and size | `lore::revision`, `lore::branch` diff calls |
| Branches: list, create, switch, merge, resolve mine/theirs | `lore::branch` |
| Locks: who holds what, lock and unlock | `lore::lock` |
| Sync and push | `lore::branch`, `lore::repository` |
| Accounts: log in, log out, current user | `lore::auth` |

The client opens any number of instances, each in its own tab, by instance path.

### Large repositories (G4)

Every list (status, history, locks) renders only the visible rows and appends rows as their events arrive,
so the client never holds a second copy of a list beyond the rows it keeps for display. Status uses the
library's modified-file tracking and does not force a full scan; a full scan is an explicit user action.

### Live updates (G5)

The client subscribes to the remote's notifications through `lore::notification` for each open instance and
refreshes the affected view (branch heads, locks) when one arrives. When notifications are unavailable, the
views show their last refresh time and refresh on focus.

## Compatibility

- **Wire format** — N/A: the client uses the library, which owns the wire format.
- **Client/server protocols** — N/A: no new RPCs; the client issues the same calls as the CLI.
- **On-disk format** — N/A: the client reads and writes instances only through the library. Its own settings
  (window layout, open instances) live in the platform's per-user configuration directory, outside any
  instance.
- **CLI and public API** — No change to `lore` subcommands or `lore-capi`. The client is a new consumer of the
  `lore` crate's Rust API; that API is not yet versioned for external consumers, so a library change that
  breaks the client is fixed in the same change, like the CLI today.

## Non-Functional Considerations

- **Concurrency** — The client serialises its own mutating operations per instance and relies on the library's
  repository lock against other processes, such as a CLI run at the same time. After any operation the client
  re-reads state from the library rather than assuming its own result.
- **Memory** — Preserved: lists are streamed from events and virtualised; the client buffers no file content
  except the diff currently shown, which it caps and pages. {TODO: diff size cap.}
- **Statelessness** — The client keeps view state for its own window only. It keeps no repository state the
  library does not own, so closing it at any time loses nothing.
- **Determinism** — Preserved: the client adds no behaviour of its own to an operation.

## Migration Plan

N/A — no breaking changes, no migration required.

## Security Considerations

The trust model does not change: the client runs as the user and has exactly the access the CLI has through the
same library and the same credential storage (`lore::auth`). It stores no tokens itself. Content from a
repository (file names, commit messages, metadata) is untrusted input: the client renders it as text only and
never interprets it as markup or runs it. {TODO: whether "open file" hands paths to the OS default handler, and
how it guards that.}

## Privacy Considerations

The client reveals nothing new to other parties: it makes the same calls the CLI makes. Locally it records the
paths of the instances the user opened, in the per-user configuration directory, and offers to clear them. It
sends no telemetry of its own; library telemetry follows the library's existing settings.

## Risks and Assumptions

**Assumptions**

- **Assumption:** The `lore` crate exposes every operation in [Views](#views-g3) with enough events to drive a
  UI. — *invalidated if:* a view needs data the library returns only as CLI text, which would need a library
  change and its own LEP first.
- **Assumption:** A pure-Rust toolkit renders large virtualised lists and text diffs well on all three
  platforms. — *invalidated if:* the prototype in phase 1 of the plan cannot keep frame time steady on a
  status list of {TODO: target size} entries.

**Risks**

- **Risk:** The `lore` Rust API changes often, and the client breaks with it. — *mitigation:* the client builds
  in CI with the workspace, so a breaking library change fails CI in the change that makes it.
- **Risk:** Epic open-sources its own client, and two clients split the effort. — *mitigation:* accepted; the
  roadmap places that release in 2027, and this client's views map onto the same API, so work can move between
  them.

## Drawbacks

- A GUI toolkit adds a large dependency tree to the workspace, which lengthens a full workspace build.
- Every new library operation gains a second consumer that may need a view.

## Alternatives Considered

### Tauri with a web frontend

A Rust backend calling `lore`, with the interface in HTML and TypeScript in a system webview.

*Rejected because:* it splits the client across two languages and a JavaScript toolchain the workspace does
not have, and the webview differs per platform. It also overlaps the [web client](../roadmap.md#web-client),
which is where a browser interface belongs.

### Electron on the existing JavaScript bindings

[ADR-00009](../developing/decisions/00009-lore-library-js-bindings.md) anticipated "future Electron
applications" on the napi bindings.

*Rejected because:* it ships a Chromium runtime per install and reaches the library through a binding layer,
where a Rust client calls it with no layer in between.

### Status quo: wait for Epic's client

*Rejected because:* the roadmap gives no date inside 2027, and until then the community has no graphical
client it can change.

## Prior Art

- **Git:** graphical clients (gitk, Sublime Merge, GitHub Desktop) are separate applications. Those that drive
  `git` by parsing its text output are the brittle pattern ADR-00009 names; Lore's library API avoids it.
- **Perforce:** P4V is the reference for lock-centric, binary-heavy workflows, with a locks and "checked out by"
  view as a first-class concept, which this client borrows.
- **Gitoxide-based tools and `gitui`:** show a Rust client driving a VCS through a library rather than a
  subprocess.

## Unresolved Questions

- Does the client start the service process when one is configured, or always call the library in-process?
- Packaging and signing per platform, and whether the release workflow builds the client.
- Should the client ship in the same release as the CLI, versioned with the workspace?
