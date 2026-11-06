# Local OIDC providers for Lore Server

Identity providers you can run on your machine to try Lore Server's OIDC authentication. Each
directory holds a Docker Compose file, the provider's configuration with test users, the
`[server.auth]` configurations that point `loreserver` at it, and a README with what is specific
to the provider.

| Directory | Provider |
| --- | --- |
| [`keycloak/`](keycloak/README.md) | [Keycloak](https://www.keycloak.org) |

Each provider runs as Tier 1, where the server reads each caller's actions from a claim in the
login token and applies them to every repository. See
[Authentication](../../docs/reference/lore-server-config.md#authentication) in the server
configuration reference for the settings.

> [!WARNING]
> These setups are for local development only:
>
> - The users and the admin accounts have fixed passwords, and the CI client has a fixed secret.
> - The providers serve plain HTTP, on `127.0.0.1` only.

## Users

Every provider has the same users:

| User | Password | Lore actions beyond `read` and `write` |
| --- | --- | --- |
| `alice@example.com` | `alice` | `admin` |
| `bob@example.com` | `bob` | `obliterate` |
| CI client | secret `lore-ci-secret` | `push-protected` |

Every token holder gets `read`, and `write` through the server's default
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
- Free ports: `41337` (TCP and UDP) and `41339` for `loreserver`, and the provider's ports, which
  its README lists.

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

The CI client logs in with the client credentials grant, with no browser:

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
Keycloak adds an `identity` claim with the user name, which users can't change there. The
revisions above then name `alice`, and the CI client's user, `service-account-lore-ci`.

## Stop

Stop `loreserver` with Ctrl+C, then in the provider's directory:

```bash
docker compose down
```

Remove `/tmp/lore-oidc-demo` to delete the server stores, the logins and the working trees.
