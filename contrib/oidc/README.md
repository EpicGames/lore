# Local OIDC providers for Lore Server

Identity providers you can run on your machine to try Lore Server's OIDC authentication. Each
directory holds a Docker Compose file, the provider's configuration with test users, the
`[server.auth]` configurations that point `loreserver` at it, and a README with what is specific
to the provider.

| Directory | Provider |
| --- | --- |
| [`keycloak/`](keycloak/README.md) | [Keycloak](https://www.keycloak.org) |
| [`dex/`](dex/README.md) | [Dex](https://dexidp.io) |
| [`authentik/`](authentik/README.md) | [authentik](https://goauthentik.io) |
| [`zitadel/`](zitadel/README.md) | [ZITADEL](https://zitadel.com) |

Each provider runs as Tier 1, where the server reads each caller's actions from a claim in the
login token and applies them to every repository, and as Tier 2, where the client exchanges its
login token for a token scoped to one repository. No provider here can issue those, so Tier 2 runs
the token exchange service in [`exchange/`](exchange/README.md) beside the provider. See
[Authentication](../../docs/reference/lore-server-config.md#authentication) in the server
configuration reference for the settings.

> [!WARNING]
> These setups are for local development only:
>
> - The users and the admin accounts have fixed passwords. The CI client has a fixed secret, or on
>   ZITADEL a generated one in `zitadel/ci-secret`.
> - The providers and the exchange service serve plain HTTP, on `127.0.0.1` only.
> - The exchange service generates its signing key at each start, so restarting it makes the
>   tokens it issued fail until the clients exchange again.
> - The Tier 2 token exchange service is an example, not meant for real deployments.

## Users

Every provider has the same users:

| User | Password | Lore actions beyond `read` and `write` |
| --- | --- | --- |
| `alice@example.com` | `alice` | `admin` |
| `bob@example.com` | `bob` | `obliterate` |
| CI client | secret `lore-ci-secret` (generated on ZITADEL) | `push-protected` (not supported by Dex in Tier 1) |

In Tier 1, every token holder gets `read`, and `write` through the server's default
`implicit_actions`. The provider's roles or groups add the rest:

- `admin`: alice can protect and unprotect branches.
- `push-protected`: the CI client can push to a protected branch. Nobody else can push to one,
  alice included.
- `obliterate`: bob can obliterate content on the server. Nobody else can.

## What you need

- Docker with Compose.
- The `lore` and `loreserver` binaries. Build them from the repository root with
  `cargo build --release`, or install them as described in
  [Install the Lore CLI](../../docs/how-to/install-lore-cli.md) and
  [Deploy a local Lore server](../../docs/how-to/deploy-local-lore-server.md).
- Free ports: `41337` (TCP and UDP) and `41339` for `loreserver`, `8190` for the token exchange
  service, and the provider's ports, which its README lists.

## Start a provider and the server

Start the provider with `docker compose up -d` in its directory, then start the server there with
the provider's configuration, as its README shows. For Keycloak:

```bash
cd contrib/oidc/keycloak
docker compose up -d
loreserver --config . --env keycloak
```

`--env keycloak` makes the server load `keycloak.toml` from the directory `--config` names. The
server keeps its stores under `/tmp/lore-oidc-demo/` and generates a self-signed certificate on
each start, as described in [Deploy a local Lore server](../../docs/how-to/deploy-local-lore-server.md).
Every provider uses the same ports, so run one at a time.

## Try branch protection

Run the steps in a second terminal. `LORE_AUTH_PATH` sets the directory of the CLI's token store,
so each user below gets their own login.

### As alice

```bash
export LORE_AUTH_PATH=/tmp/lore-oidc-demo/alice
lore auth login lore://localhost:41337
```

Open the URL the CLI prints, sign in as `alice@example.com` with password `alice`, and grant the
access the page asks for, if it asks. Then create a repository, push a first revision and protect
`main`:

```bash
mkdir /tmp/lore-oidc-demo/alice-work && cd /tmp/lore-oidc-demo/alice-work
lore repository create lore://localhost:41337/demo
echo one > a.txt
lore stage a.txt
lore commit "First revision"
lore push
lore branch protect main
```

Commit again and push. The server refuses the push, because alice doesn't hold `push-protected`:

```bash
echo two >> a.txt
lore stage a.txt
lore commit "Second revision"
lore push
```

```text
[Error] Not authorized to access repository
```

### As bob

In a new terminal:

```bash
export LORE_AUTH_PATH=/tmp/lore-oidc-demo/bob
lore auth login lore://localhost:41337
```

Sign in as `bob@example.com` with password `bob`. Then clone, commit and push:

```bash
cd /tmp/lore-oidc-demo
lore clone lore://localhost:41337/demo bob-work && cd bob-work
echo bob > b.txt
lore stage b.txt
lore commit "Bob's revision"
lore push
lore branch unprotect main
```

The server refuses both. `lore branch unprotect` needs `admin`, unless you created the repository.

### As the CI client

The CI client logs in with the client credentials grant, with no browser. On Keycloak, authentik
and ZITADEL its client ID is `lore-ci`. ZITADEL generates the secret, so on ZITADEL replace
`lore-ci-secret` below with `$(cat <repository>/contrib/oidc/zitadel/ci-secret)`. Dex can't give a
client a role, so its CI client works in [Tier 2](#tier-2) only, under the client ID its
[README](dex/README.md#the-ci-client) names.

```bash
export LORE_AUTH_PATH=/tmp/lore-oidc-demo/ci
LORE_CLIENT_SECRET=lore-ci-secret lore auth login lore://localhost:41337 --client-id lore-ci
cd /tmp/lore-oidc-demo
lore clone lore://localhost:41337/demo ci-work && cd ci-work
echo ci > c.txt
lore stage c.txt
lore commit "CI revision"
lore push
```

The push to protected `main` succeeds.

## Try obliterate

Bob holds `obliterate`, so he can remove content from the server. `main` is protected, so in
bob's terminal, in `bob-work`, create a branch, commit a file to it, push it, and obliterate the
file:

```bash
lore branch create bob-work
echo "password=hunter2" > leaked.txt
lore stage leaked.txt
lore commit "Add leaked.txt"
lore push
lore file obliterate --path leaked.txt
```

The client warns that clones and syncs that need the content will fail, then removes it from the
local store and from the server. In alice's terminal, `lore branch switch bob-work` in
`alice-work` fails because the server no longer has the content of `leaked.txt`.

Alice doesn't hold `obliterate`, and the server refuses her `lore file obliterate` with
`Not authorized to access repository`. The client removes the content from her local store before
it asks the server, so don't try this on content you want to keep.

## Who the revisions name

The server takes the caller's identity from the token's `identity` claim, and from `sub` when the
token has none. The client records the same value as the creator and committer of each revision.
Keycloak and authentik add an `identity` claim with the user name, which users can't change
there. On ZITADEL, an Action adds one with the user ID, which setup makes the same as the user
name. The revisions above then name `alice`, and the CI client's user, such as
`service-account-lore-ci` on Keycloak. Dex can't add the claim, so revisions there name its `sub`,
which is stable too. The grants of [Tier 2](#tier-2) name users by the same identity.

## Tier 2

In Tier 2 the client logs in at the provider, then exchanges its login token (RFC 8693) for a token
scoped to the one repository it's working on, at the token exchange service in
[`exchange/`](exchange/README.md). The service decides who may do what on each repository, from
`grants.json` in the provider's directory, in place of the provider's roles or groups. The grants
name two repositories by fixed IDs, which `lore repository create --id` gives them:

| User | `game`, ID `11111111111111111111111111111111` | `tools`, ID `22222222222222222222222222222222` |
| --- | --- | --- |
| alice | `write`, `admin` | `write`, `admin` |
| bob | `write`, `obliterate` | `read` |
| CI client | `write`, `push-protected` | none |

Alice's grants are on `*`, every repository. A user with no grant on a repository can't reach it
at all, and any grant on a repository lets the user read it.

Start the provider with the exchange service beside it, and the server with the provider's Tier 2
configuration, `<provider>-tier2.toml`. For Keycloak:

```bash
cd contrib/oidc/keycloak
docker compose --profile tier2 up -d
loreserver --config . --env keycloak-tier2
```

The server fetches the service's keys when it starts, and doesn't start if the service isn't up
yet.

### Tier 2 as alice

```bash
export LORE_AUTH_PATH=/tmp/lore-oidc-demo/alice
lore auth login lore://localhost:41337
```

Open the URL the CLI prints, sign in as `alice@example.com` with password `alice`, and grant the
access the page asks for, if it asks. Then create both repositories, push to each, and protect
`main` in `game`:

```bash
for repo in game:11111111111111111111111111111111 tools:22222222222222222222222222222222; do
  mkdir /tmp/lore-oidc-demo/alice-${repo%%:*} && cd /tmp/lore-oidc-demo/alice-${repo%%:*}
  lore repository create lore://localhost:41337/${repo%%:*} --id ${repo##*:}
  echo one > a.txt
  lore stage a.txt
  lore commit "First revision"
  lore push
done
cd /tmp/lore-oidc-demo/alice-game
lore branch protect main
```

### Tier 2 as bob

In a new terminal:

```bash
export LORE_AUTH_PATH=/tmp/lore-oidc-demo/bob
lore auth login lore://localhost:41337
```

Sign in as `bob@example.com` with password `bob`. In `game`, bob can't push to the protected
`main`, but he can push to a branch of his own, and [obliterate](#try-obliterate) there:

```bash
cd /tmp/lore-oidc-demo
lore clone lore://localhost:41337/game bob-game && cd bob-game
lore branch create bob-work
echo bob > b.txt
lore stage b.txt
lore commit "Bob's revision"
lore push
```

In `tools`, bob can clone, but the server refuses his push, even to a branch of his own:

```bash
cd /tmp/lore-oidc-demo
lore clone lore://localhost:41337/tools bob-tools && cd bob-tools
lore branch create bob-work
echo bob > b.txt
lore stage b.txt
lore commit "Bob's revision"
lore push
```

### Tier 2 as the CI client

In a new terminal, log in, clone `game`, and push to its protected `main`. On Dex, the client ID is
`https://localhost/ci` instead of `lore-ci`. On ZITADEL, replace `lore-ci-secret` as in
[As the CI client](#as-the-ci-client):

```bash
export LORE_AUTH_PATH=/tmp/lore-oidc-demo/ci
LORE_CLIENT_SECRET=lore-ci-secret lore auth login lore://localhost:41337 --client-id lore-ci
cd /tmp/lore-oidc-demo
lore clone lore://localhost:41337/game ci-game && cd ci-game
echo ci > c.txt
lore stage c.txt
lore commit "CI revision"
lore push
```

The push succeeds. The CI client has no grant on `tools`, so cloning it fails with
`Not authorized to access repository`:

```bash
cd /tmp/lore-oidc-demo
lore clone lore://localhost:41337/tools ci-tools
```

### Change a grant

The service reads `grants.json` on every exchange, but the client keeps an exchanged token until it
expires, within five minutes, and exchanges again only then. A grant you add or remove therefore
applies to a user within five minutes. Give bob `write` on `tools` under his identity, `bob`, or his
`sub` on Dex, and his push there succeeds once his current token for `tools` has expired:

```json
"22222222222222222222222222222222": ["write"]
```

## Stop

Stop `loreserver` with Ctrl+C, then in the provider's directory:

```bash
docker compose --profile tier2 down
```

Remove `/tmp/lore-oidc-demo` to delete the server stores, the logins and the working trees.
