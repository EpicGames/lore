# ZITADEL

[ZITADEL](https://zitadel.com) 4.20, self-hosted, on `http://localhost:8280`, set up by `setup.js`
through ZITADEL's API. See the [walkthrough](../README.md) for what to try.

## Start

Tier 1:

```bash
docker compose up -d
docker compose wait setup
loreserver --config . --env zitadel
```

Tier 2, with the [token exchange service](../exchange/README.md) on `http://localhost:8190`:

```bash
docker compose --profile tier2 up -d
docker compose wait setup
loreserver --config . --env zitadel-tier2
```

ZITADEL runs with Postgres, and takes about half a minute to start. The `setup` container then
sets it up and exits, and `docker compose wait setup` waits for that. If setup fails,
`docker compose logs setup` names the call that failed, and the next `docker compose up -d`
completes it.

The admin console is at `http://localhost:8280/ui/console`, user `admin@lore.localhost`, password
`admin`. ZITADEL adds the organization's domain to the first admin's user name.

## Generated client ID and secret

ZITADEL generates the CLI client's ID and the CI client's secret. `setup.js` writes them to two
files in this directory, which `.loreignore` keeps out of the repository:

- `local.toml` sets `[server.auth.oidc] client_id`. `loreserver --config .` loads it after the
  configuration `--env` names, so `zitadel.toml` and `zitadel-tier2.toml` don't set it.
- `ci-secret` holds the CI client's secret. Log the CI client in with it, from this directory:

  ```bash
  LORE_CLIENT_SECRET=$(cat ci-secret) lore auth login lore://localhost:41337 --client-id lore-ci
  ```

After `docker compose down`, the next `docker compose up -d` starts ZITADEL with a new database,
and setup writes both files anew. Start the server again after that, and log the users in again.
`docker compose --profile tier2 down -v` also removes the volumes that `down` leaves behind.

## The setup

| User | Project roles |
| --- | --- |
| `alice` (`alice@example.com`) | `admin` |
| `bob` (`bob@example.com`) | `obliterate` |
| `lore-ci`, a machine user | `push-protected` |

- The project `lore-project` has roles named after the Lore actions: `admin`, `obliterate` and
  `push-protected`.
- `lore-cli` is the native app the CLI logs in with, by the device grant. After sign-in, ZITADEL
  asks the user to allow the device. `lore-ci` is a machine user, which logs in with the client
  credentials grant under its user name as the client ID.
- Both get JWT access tokens. ZITADEL issues opaque ones by default.
- Each user's ID is their user name when setup creates it. The ID is the same on every setup, and
  unlike the user name, users can't change it.
- The login page accepts the user name or the email address. The password rules are relaxed so
  that the passwords can be the user names, and the login doesn't offer to set up a second factor.

### The Action

ZITADEL's own claims don't fit what Lore reads:

- `aud` holds only project and client IDs. The client sends a token only to a host that its `aud`
  or `root_domains` names.
- The roles claim is an object keyed by role. `permission_claim` reads a list.

An Action, `loreClaims`, runs before ZITADEL creates each access token and adds three claims:

- `root_domains`, `["localhost"]`: the host the client may send the token to.
- `identity`: the user ID, which revisions record as the creator and committer, and
  `grants.json` names users by.
- `lore_actions`: the user's roles in `lore-project`. `zitadel.toml` reads it as
  `permission_claim`.

The server names the project in `jwt_audience`.

The Action is a ZITADEL v1 Action, which runs inside ZITADEL. ZITADEL 4 deprecates v1 Actions in
favour of v2 Actions, which call an HTTP service of your own. A ZITADEL release without v1 Actions
needs such a service to add the claims.

## Scopes

ZITADEL adds the project to `aud` only for the scope
`urn:zitadel:iam:org:project:id:lore-project:aud`. It gives the Action the user's roles only for
`urn:zitadel:iam:org:projects:roles`. `zitadel.toml` names both in `scopes`, for the device login,
and in `client_credentials_scopes`, for the CI client. `zitadel-tier2.toml` names only the first,
because in Tier 2 `grants.json` holds the grants. A client from before `client_credentials_scopes`
requests no scope with the client credentials grant. Its CI token's `aud` then names only
`lore-ci`, and the server refuses it.
