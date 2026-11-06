# Keycloak

[Keycloak](https://www.keycloak.org) 26.4 in development mode, on `http://localhost:8180`, with the
realm `lore` imported from `realm.json`. See the [walkthrough](../README.md) for what to try.

## Start

Tier 1:

```bash
docker compose up -d
loreserver --config . --env keycloak
```

Tier 2, with the [token exchange service](../exchange/README.md) on `http://localhost:8190`:

```bash
docker compose --profile tier2 up -d
loreserver --config . --env keycloak-tier2
```

Keycloak takes up to a minute to start. Its admin console is at `http://localhost:8180/admin`, user
`admin`, password `admin`. `KC_HOSTNAME` in `compose.yaml` pins the issuer to
`http://localhost:8180/realms/lore`, which `keycloak.toml` names as `jwt_issuer`.

## The realm

| User | Realm roles |
| --- | --- |
| `alice` (`alice@example.com`) | `admin` |
| `bob` (`bob@example.com`) | `obliterate` |
| `service-account-lore-ci` | `push-protected` |

- `lore-cli` is the public client the CLI logs in with, by the device grant. Its access tokens are
  full JWTs, not lightweight ones. The device login asks the user to grant access before it
  completes.
- `lore-ci` is the confidential client for CI, with the client credentials grant and the secret
  `lore-ci-secret`. Its service account user holds `push-protected`.
- Realm roles reach the token in `realm_access.roles`, which `keycloak.toml` reads as
  `permission_claim`. The server ignores the roles that aren't Lore action names.
- An audience mapper on both clients adds `https://localhost` to `aud`. The client sends a token
  only to a server whose host the token's `aud` names.
- An identity mapper on both clients sets the `identity` claim to the user name, which users
  can't change, unlike their email address. Revisions record it as the creator and committer, and
  `grants.json` names users by it.

In Tier 2, `grants.json` holds the grants in place of the realm roles. Keycloak can't issue the
repository-scoped tokens itself: its token exchange refuses public clients such as `lore-cli`.
