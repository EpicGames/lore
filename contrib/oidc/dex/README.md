# Dex

[Dex](https://dexidp.io) v2.46.1, on `http://localhost:5556`, with the issuer
`http://localhost:5556/dex` and its state in memory. See the [walkthrough](../README.md) for what to
try.

## Start

Tier 1:

```bash
docker compose up -d
loreserver --config . --env dex
```

Tier 2, with the [token exchange service](../exchange/README.md) on `http://localhost:8190`:

```bash
docker compose --profile tier2 up -d
loreserver --config . --env dex-tier2
```

## The configuration

| User | Groups |
| --- | --- |
| `alice` (`alice@example.com`) | `admin` |
| `bob` (`bob@example.com`) | `obliterate` |

- `lore-cli` is the public client the CLI logs in with, by the device grant. Dex's device callback
  is registered as its redirect URI, which the device grant needs.
- The users' `groups` reach the token as the flat `groups` claim, which `dex.toml` reads as
  `permission_claim`. Dex adds it only when the client requests the `groups` scope, and reads
  `groups` on static passwords from v2.46.0.
- Dex puts only client IDs in `aud`. The public client `https://localhost` exists so that the
  cross-client scope `audience:server:client_id:https://localhost` adds `https://localhost` to the
  token's `aud`, which the Lore client checks before sending the token to the server.
- `dex.toml` advertises the scopes `openid`, `profile`, `groups` and the cross-client scope.
  `dex-tier2.toml` leaves out `groups`, since the exchange service holds the actions.

## Who the revisions name

Dex has no claim mappers, so it can't add an `identity` claim, and revisions name Dex's `sub`. It
encodes the user ID and the connector, such as `CgVhbGljZRIFbG9jYWw` for alice and
`CgNib2ISBWxvY2Fs` for bob, and Tier 2's `grants.json` names users by it. The value follows from
`userID` in `config.yaml` and the password database's connector ID, `local`, so it's the same on
every machine. For a user you add, it's the unpadded base64url encoding of the protobuf message
with the user ID as field 1 and `local` as field 2. `lore auth list` and
`lore history` show `alice` for your own login, from the `name` claim the `profile` scope adds.

## The CI client

Dex can't give a client a role: a token from the client credentials grant carries no `groups`. So
the CI client works in Tier 2 only, where its grants come from `grants.json`. It differs from the
other providers' in two ways:

- A Dex client credentials token carries only the client ID in `aud`, and the Lore client sends a
  token only to a host its `aud` names. The client ID is therefore a URL on the server's host,
  `https://localhost/ci`, and `dex-tier2.toml` accepts it in `jwt_audience`:

  ```bash
  LORE_CLIENT_SECRET=lore-ci-secret lore auth login lore://localhost:41337 --client-id https://localhost/ci
  ```

- `grants.json` and revisions name the client by the `sub` Dex derives from its ID,
  `ChRodHRwczovL2xvY2FsaG9zdC9jaQ`.
