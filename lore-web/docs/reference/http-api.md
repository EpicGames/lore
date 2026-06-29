# lore-web HTTP API

The lore-web server exposes a small JSON + streaming API on `127.0.0.1:7420`
(configurable). The browser SPA is its only intended client. All repository data
is read live from the Lore SDK on each request; nothing is cached server-side.

## Conventions

- Request and response bodies are JSON unless noted.
- Errors return a non-2xx status with `{ "error": "<message>" }`, where the
  message is the underlying Lore failure detail.
- `path` is an absolute path to a Lore working copy. Pass it as a query
  parameter on GETs and in the body on POSTs.

## Endpoints

### Repositories

| Method | Path | Body / query | Description |
| --- | --- | --- | --- |
| GET | `/api/repos` | — | List tracked repos, each enriched with live `branch` and `exists`. |
| POST | `/api/repos` | `{ path, label }` | Start tracking a working copy. Rejects non-repos. |
| DELETE | `/api/repos` | `{ path }` | Stop tracking. Always succeeds, even if the folder is gone. |

### Reads

| Method | Path | Query | Description |
| --- | --- | --- | --- |
| GET | `/api/status` | `path` | Current branch plus staged/unstaged changed files. Also returns `hasLoreignore`/`hasGitignore` flags, and marks each changed entry that is itself a nested Lore working copy with `nested: true`. |
| GET | `/api/history` | `path`, `length` | Revision history (default 50), with message and timestamp. |
| GET | `/api/branches` | `path` | Branch list. |
| GET | `/api/diff` | `path`, `file` | Unified diff for one file. |
| GET | `/api/auth` | — | `{ loggedIn }` — whether the CLI has a stored identity. |

### Writes

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| POST | `/api/stage` | `{ path, files }` | Stage the given files. |
| POST | `/api/unstage` | `{ path, files }` | Unstage the given files. |
| POST | `/api/reset` | `{ path, files }` | Discard working changes to the given files. |
| POST | `/api/commit` | `{ path, message }` | Commit the staged revision. |
| POST | `/api/ignore` | `{ path, pattern }` | Append a gitignore-style `pattern` (file, `folder/`, or `*.ext`) to `.loreignore`, creating it if absent. Returns `{ ok, added }`. |
| POST | `/api/init-loreignore` | `{ path }` | Set up `.loreignore` (seeded from `.gitignore` when present) and keep each tool's metadata out of the other's history. Returns `{ ok, created, gitignoreUpdated }`. |
| POST | `/api/repair` | `{ path }` | Rebuild the working copy's `.lore` in place to purge unremovable stale index entries, preserving the repository id and remote. Refused (409) when there is committed history. Returns `{ ok, id }`. |

### Remote operations (streamed)

These respond with `application/x-ndjson`: one normalized Lore event per line,
ending with a `{ "tag": "DONE", "data": { "ok", "status", "message" } }` marker.

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| POST | `/api/sync` | `{ path, revision?, reset? }` | Sync the working copy to a revision. |
| POST | `/api/push` | `{ path, branch?, fastForwardMerge? }` | Push commits to the remote. |
| POST | `/api/clone` | `{ url, dest }` | Clone a remote repository into `dest`. |

### Events

| Method | Path | Description |
| --- | --- | --- |
| GET | `/events` | Server-Sent Events. Emits `{ "type": "refresh", "repo", "reason" }` when a tracked repo changes on disk, so the SPA refetches. |

## See also

- [How to run lore-web](../how-to/run-lore-web.md)
- [Architecture](../explanation/architecture.md)
