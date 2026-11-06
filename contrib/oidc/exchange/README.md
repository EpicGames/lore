# Token exchange service

An example of a token exchange service (RFC 8693) for Lore's Tier 2, in one dependency-free
Node.js file, `server.js`. It runs beside an OIDC provider that handles login, and issues the
tokens scoped to one repository that the provider can't. `docker compose --profile tier2 up -d` in a provider's
directory starts it on `127.0.0.1:8190`.

> [!WARNING]
> This service isn't meant for real deployments. It shows what a token exchange service has to do
> for Lore, for local development and testing. It generates its signing key at startup and keeps it
> in memory, reads its grants from a JSON file, and has no administration, audit log, high
> availability or key rotation. A real deployment runs a token service built for production,
> following the same contract.

## What it does

1. The client sends the login token as `subject_token` and the repository as
   `resource=https://localhost/partitions/<ID>`.
2. The service checks that the login token's `iss` is one of the provider's issuers, its signature
   against that issuer's keys, which it fetches through the issuer's discovery document, and its
   `aud` and `exp`.
3. It looks the caller up in the grants file by the identity the Lore server reads from the
   token: its `identity` claim, the alias `https://lore.org/claims/identity`, or else `sub`.
4. It answers with a token of type `at+jwt` signed with its own key, valid for at most five
   minutes and never past the login token. The token carries the login token's identity, so the
   caller is the same user to the Lore server in both tokens, and
   `resources: [{"resource_id": "urc-<ID>", "permission": [...]}]`.

It refuses a login token it can't verify with `invalid_grant`, which the Lore client reports as
not authenticated, and a repository the caller has no grant on with `invalid_target`, which it
reports as not authorized. When it can't fetch the provider's keys it answers `server_error`.

The Lore server names the service as `[server.auth.oidc] token_exchange_issuer`, finds the service's
keys through its discovery document at `/.well-known/openid-configuration`, and verifies the
service's tokens against those keys only. It still receives login tokens, for calls that don't
name one repository such as `lore repository create`, and verifies those against the provider's
keys.

## The grants file

The file maps a user to the actions they hold, per repository ID, with `*` for every repository:

```json
{
  "alice": {
    "*": ["write", "admin"]
  },
  "bob": {
    "<repository ID>": ["write"]
  }
}
```

A user's actions on a repository are those under `*` and under its ID together. Any grant on a
repository also lets the user read it, so `["read"]` grants reading alone. The service ignores, and
logs, an action name Lore doesn't have. The service reads the file on every exchange, but the client
keeps an exchanged token until it expires, so a change applies to a user within five minutes.

## Settings

The service reads its settings from the environment:

| Variable | Default | Description |
| --- | --- | --- |
| `LOGIN_ISSUERS` | required | Comma-separated issuers, one of which a login token must carry as `iss`. More than one for a provider that gives each client an issuer of its own. The service fetches each issuer's keys through its discovery document. |
| `PROVIDER_URL` | none | Where the service reaches the provider, when that differs from the host the issuers name, such as the provider's host name inside Docker. Replaces the scheme, host and port of the discovery and key URLs. A provider that names its issuer by the host a request reaches it on, such as authentik, then names it by this host, so the service compares only the issuer's path with the discovery document. A login token's `iss` still has to match `LOGIN_ISSUERS` exactly. |
| `LOGIN_AUDIENCES` | `AUDIENCE` | Comma-separated `aud` values, one of which a login token must carry. |
| `AUDIENCE` | `https://localhost` | The `aud` of the tokens the service issues, which the Lore server accepts in `jwt_audience`. |
| `RESOURCE_PREFIX` | `https://localhost/partitions/` | The prefix of the `resource` before the repository ID. It matches the server's `resource_template`. |
| `RESOURCE_ID_TEMPLATE` | `urc-{id}` | The `resource_id` the service issues, `{id}` standing for the repository ID. It matches the server's `resource_id_template`. |
| `GRANTS_FILE` | required | The grants file. |
| `ISSUER` | `http://localhost:8190` | The service's own issuer, which the Lore server names as `token_exchange_issuer`. |
| `HOST` | `127.0.0.1` | The address the service listens on. The compose files set `0.0.0.0` inside the container and publish the port on `127.0.0.1`. |
| `PORT` | `8190` | The port the service listens on. |

## Tests

`server.test.js` tests the service as a process against a stub provider, over HTTP, with Node's
built-in test runner and no dependencies. CI doesn't run them. Run them in this directory with
Node.js 22 or later:

```bash
node --test
```

or in Docker:

```bash
docker run --rm -v "$PWD":/srv -w /srv node:22-alpine node --test
```

One test waits ten seconds, the time the service waits before it fetches the provider's keys again
for an unknown key ID.
