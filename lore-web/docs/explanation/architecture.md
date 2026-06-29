# Architecture

This page explains how lore-web is built and why it avoids the staleness and
delete bugs of the official desktop client.

## The shape of the app

lore-web is a small Node process with two halves:

- A **server** (`server/`) built on Node's standard library only — `http` for
  routing, `fs.watch` for file watching, a JSON file for the tracked-repo list.
  Its one non-stdlib dependency is `@lore-vcs/sdk` (from npm), which loads the
  native `lorelib` through koffi (FFI). No web framework, no database, no build.
- A **browser SPA** (`web/`) of plain ES modules — no framework, no bundler.

The browser talks to the server over JSON and two streaming channels: Server-Sent
Events for change notifications, and newline-delimited JSON for live operation
progress.

## The engine

Every Lore operation flows through `server/sdk.mjs`, which wraps the SDK's fluent
API (`lore.<verb>(globalArgs, args)`). Two helpers cover all needs: `collect()`
runs a verb to completion and returns its events; `stream()` yields events as
they arrive for long operations. The SDK reads working copies directly on disk,
so no running server is required for local reads and writes — only remote
operations (`clone`, `push`, `sync`) contact a Lore server, whose address comes
from the repository's own config.

Failures follow Lore's error contract: a verb's outcome is canonical on its
`COMPLETE` event status, with human-readable detail on `ERROR` events. The
wrapper captures both and raises one typed error, which the routes surface as a
clean message — never a swallowed exception.

## Why it stays fresh

The desktop client this replaces persists its whole UI state to SQLite and
rehydrates on launch, and watches nothing. The result: repository and revision
lists reflect a past snapshot until you restart or clear the cache.

lore-web inverts that. **It persists no view state** — only the set of repo
paths you've added. Every view is fetched live, and four triggers keep it
current:

1. **On demand** — selecting a repo, or any explicit refresh, refetches.
2. **On disk change** — `fs.watch` on each repo's working tree and `.lore`
   directory emits a scoped SSE `refresh`; the SPA refetches the affected views.
   This catches revisions committed by the CLI or arriving from a push.
3. **On focus** — regaining the window refetches, covering anything missed while
   the app was in the background.
4. **On a slow poll** — history refetches periodically, catching revisions a
   collaborator pushed to the server (a change with no local filesystem event).

## Why deleting a missing repo works

Removing a repository only edits the tracked-repo list; it never depends on the
folder still existing. A dangling entry is always removable — the opposite of the
desktop client, which ran a repository-status check first and refused to remove
the entry when that check failed on a missing folder.

## What's intentionally out of scope

A repository's organization is the `org/` prefix of its `name` metadata, set from
the path of the create or clone URL. lore-web reads it through the SDK's
`repositoryMetadataGet` verb and surfaces it per repository. Lore makes `name`
read-only after creation, so changing the organization is not a metadata edit: it
rebuilds the working copy's `.lore` under a new URL (preserving the repository id
and remote), the same mechanism repository repair uses. That rebuild discards
local committed revisions, so the UI confirms the loss before proceeding and the
repository should be fully pushed to its remote first.

Renaming an organization across a hosted account — which would retag every repo it
owns — has no verb in the Lore SDK or the open-source server and stays out of
scope.

## Related

- [HTTP API reference](../reference/http-api.md)
- [How to run lore-web](../how-to/run-lore-web.md)
