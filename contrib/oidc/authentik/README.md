# authentik

[authentik](https://goauthentik.io) 2026.8.5, on `http://localhost:9000`, set up from
`blueprint.yaml`. See the [walkthrough](../README.md) for what to try.

## Start

Tier 1:

```bash
docker compose up -d
loreserver --config . --env authentik
```

Tier 2, with the [token exchange service](../exchange/README.md) on `http://localhost:8190`:

```bash
docker compose --profile tier2 up -d
loreserver --config . --env authentik-tier2
```

authentik runs as three containers: Postgres, the server and a worker. It takes about a minute to
start, and the server can't start until authentik's discovery document answers at
`http://localhost:9000/application/o/lore-cli/.well-known/openid-configuration`. The worker applies
`blueprint.yaml` on start, so nothing is set up by hand. The admin interface is at
`http://localhost:9000/if/admin/`, user `akadmin`, password `admin`.

The login page accepts the user name or the email address, and asks for no consent.

## The blueprint

| User | Groups |
| --- | --- |
| `alice` (`alice@example.com`) | `admin` |
| `bob` (`bob@example.com`) | `obliterate` |
| `ak-lore-ci-client_credentials` | `push-protected` |

- Groups are named after the Lore actions: `admin`, `obliterate` and `push-protected`.
- A scope mapping for the scope `lore` sets `aud` to `https://localhost`, `identity` to the user
  name, and `lore_actions` to the user's groups that name a Lore action. authentik doesn't let
  users change their user name by default, so revisions and `grants.json` name users by it.
  `authentik.toml` reads `lore_actions` as `permission_claim`.
- `lore-cli` is the public client the CLI logs in with, with the device code and refresh token
  grants. `lore-ci` is the confidential client for CI, with the client credentials grant and the
  secret `lore-ci-secret`. authentik issues `lore-ci`'s tokens for the user
  `ak-lore-ci-client_credentials`.
- An empty device code flow is set on the default brand. Without one, authentik answers the device
  login URL with 404.

## Scopes

authentik adds the `lore` scope mapping's claims only when the client requests the scope, and has
no default scopes. `authentik.toml` therefore names `lore` in both `scopes`, for the device login,
and `client_credentials_scopes`, for the CI client. A client from before
`client_credentials_scopes` requests no scope with the client credentials grant, and its CI login
fails with `JWT names no domain it may be sent to`.

## Differences from Keycloak

- **An issuer per client.** Each authentik application has its own issuer,
  `http://localhost:9000/application/o/<slug>/`, with a trailing slash. `jwt_issuer` lists both
  `lore-cli`'s and `lore-ci`'s, and so does the exchange service's `LOGIN_ISSUERS`. This works
  because both sign with the same key, and the server finds that key through the first issuer.
- **An issuer from the request's host.** authentik names its issuer by the host a request reaches
  it on, so the exchange service, which reaches it as `server:9000`, sees `server:9000` in the
  discovery document. `PROVIDER_URL` maps that host back.
- **A throttled device endpoint.** authentik answers 20 device login requests per hour from one
  client address. Logging in many times from one machine can reach the limit.
- **A numeric user code.** The device login URL carries a number, not a code like `ABCD-EFGH`.
