# lore-web

A self-hosted browser UI for the [Lore](https://epicgames.github.io/lore/) version
control system. It drives Lore through the same `@lore-vcs/sdk` engine the
official desktop app uses, but with refresh logic that never serves a stale
cache — lists update live from disk and after every action.

It runs identically on two kinds of machine:

- **Host** — a machine with a local `lore-server`. lore-web manages local working
  copies and is the push/sync target for collaborators.
- **Collaborator** — a machine with no server. lore-web drives `clone` / `sync` /
  `push` against a host's server over the network. It needs only the SDK
  dependency and a one-time `lore login`.

## Why this exists

The official desktop app persists its entire UI state to SQLite and rehydrates
it on launch, with no file watching — so repository and revision lists go stale
until you restart or clear the cache, and it refuses to remove a repository whose
folder was deleted. lore-web fixes these by owning the refresh path: see
[docs/explanation/architecture.md](docs/explanation/architecture.md).

## Quick start

### Windows (no terminal needed)

1. Double-click **`setup.bat`** — checks for Node.js and the `lore` CLI (offering
   to install anything missing) and installs the SDK.
2. Double-click **`start.bat`** — launches the app and opens
   `http://127.0.0.1:7420`. (It also runs setup automatically on first use, so you
   can skip step 1 and run `start.bat` directly.)

### Any platform (terminal)

```sh
cd lore-web
npm install       # pulls @lore-vcs/sdk (and its native lorelib) from npm
npm start         # launch the server and open http://127.0.0.1:7420
```

Then click **Add**, paste the path to a Lore working copy, and you're in.

> **Sharing with a collaborator:** the `lore-web` folder is self-contained — send
> only that folder (no need to clone this repository). They run `setup.bat` (or
> `npm install`) once, then `lore login lore://<your-host>:41337`. See the
> [how-to guide](docs/how-to/run-lore-web.md).

- Run headless (no browser auto-open): `npm run serve`
- Run the tests: `npm test`
- Smoke-test the SDK against a repo: `npm run smoke -- "D:\path\to\repo"`

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `LORE_WEB_PORT` | `7420` | HTTP port |
| `LORE_WEB_HOST` | `127.0.0.1` | bind address — keep it loopback |
| `LORE_WEB_LOG_LEVEL` | `info` | `trace`…`error` |
| `LORE_WEB_STORE` | `~/.lore-web/store.json` | tracked-repo list location |
| `LORE_WEB_DEFAULT_REMOTE` | none | initial remote server URL, seen once on first run |
| `LORE_CLI` | `lore` | path to the `lore` CLI (login/service fallback) |

The remote server (the `lore://host:port` a new repository is created under, and
where **Server repositories…**/**Clone from URL…** look) is set from the app
itself: click the **⚙** button beside the `lore web` logo. This is how a
collaborator points lore-web at their host's server — see
[Set up a collaborator](docs/how-to/run-lore-web.md#set-up-a-collaborator-no-server).
`LORE_WEB_DEFAULT_REMOTE` only seeds the value before it has ever been set
through the app; once configured, the app's own setting takes over.

> **Security:** lore-web exposes full read/write access to your repositories and
> is bound to loopback only. Never expose it on a network. The *Lore server* is
> the networked component, not lore-web.

## The SDK dependency

lore-web does not depend on any other Lore client. Its one runtime dependency is
[`@lore-vcs/sdk`](https://www.npmjs.com/package/@lore-vcs/sdk) from the public npm
registry; `npm install` also pulls the platform-specific native library
(`lorelib`) and `koffi` automatically. No binaries are committed to the repo.

Keep the SDK **version-matched to the Lore server** you talk to (currently
`0.8.4`). To upgrade, bump the version in `package.json` and re-run `npm install`.

## Documentation

- [How to run lore-web](docs/how-to/run-lore-web.md) — setup, and collaborator login
- [HTTP API reference](docs/reference/http-api.md) — every endpoint
- [Architecture](docs/explanation/architecture.md) — how it works and why
