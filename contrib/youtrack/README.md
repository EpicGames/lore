# Connect Lore to YouTrack

This optional bridge sends reachable Lore revisions to YouTrack's **Generic VCS**
integration. Run it beside your Lore Server and YouTrack Server on your own
hardware. It uses Python 3.11 or later, the standard library, and the official
Lore CLI. No Python packages or changes to the Lore wire protocol are required.

The implementation is maintained in this fork. The source review used upstream
`b8fc623d5b064e910012957c58dc6d3597c45b41` (`0.12.1-nightly`); native integration
tests use the official `0.12.0+1967` client and server. Unknown JSON schemas fail
before delivery. See [implementation research](RESEARCH.md) for the comparison
of Gitea, GitLab, and open source YouTrack clients, with pinned sources.

## Behavior

The bridge reads remote branches and revision metadata through a dedicated bare
clone. It follows both revision parents and branch stack points, including
intermediate revisions in a multi-revision push. It does not check out asset
files or send file contents to YouTrack.

Each request contains one revision: its immutable signature, original author,
original timestamp in milliseconds, message, and the observed branch. Lore
identities are mapped explicitly to YouTrack usernames or email addresses.
YouTrack handles issue references and any enabled commands itself. The bridge
does not add comments or change issue states through a second API.

The SQLite database binds to one repository ID and one receiver URL. A revision
is queued **once across the configured branches**, not once per branch. With
several matching branches, their names and IDs are sorted, and the first observed
branch supplies the revision's branch attribution. A later merge does not replay
the original revision's commands. Use `branches = ["main"]` when commands should
run only after changes reach the main branch; matching merge ancestors then
receive the main branch attribution. This is an explicit policy, not a complete
branch-membership export.

Branch names are sent as `refs/heads/<Lore branch name>`. Configure matching
YouTrack branch filters. The bridge's `branches` setting uses case-sensitive
shell patterns over the original Lore names, such as `main` or `release/*`.
If you broaden the bridge filter, broaden YouTrack's filter too: a revision
accepted by HTTP but filtered out as `feature` will not be sent again as `main`.

## Configure YouTrack

1. Create a **Generic** VCS integration for your project. Set its repository URL
   template to your revision viewer's URL with `$hash$` as the placeholder.
2. Copy its generated **Resource URL** and **Auth Header**. The bridge sends the
   Auth Header unchanged as `X-YT-Simple-Signature`; it is not an HMAC signature.
3. Select **Add commits, ignore commands** for the first connection. If commands
   are needed, configure YouTrack's processing scheme, author mappings, and
   allowed committers before enabling them.

See the official [Generic VCS setup](https://www.jetbrains.com/help/youtrack/server/generic-vcs-integration.html)
and [Auth Header contract](https://www.jetbrains.com/help/youtrack/cloud/integrate-project-with-generic-vcs.html).
The bridge cannot configure these settings or verify their current values with
the receiver secret. An HTTP 2xx response confirms HTTP acceptance; inspect a
real issue to qualify the connection to your YouTrack deployment.

Lore's core server does not provide a revision web viewer. This bridge does not
add one. The repository URL configured in YouTrack controls the links it shows.
Use a URL that your team can open; a nonexistent example URL produces a broken
link even if revision attachment succeeds.

## Configure the bridge

Use a service account with read access to the selected Lore repository. Set up
its Lore authentication using the normal Lore CLI. Keep the clone, configuration,
credentials, and database in directories owned by the service administrator.
Never load receiver settings from game content or an untrusted working tree.

Create a dedicated bare clone:

```bash
lore repository clone lores://lore.example.com/game /var/lib/lore-youtrack/cache --bare
```

Copy [config.example.toml](config.example.toml) to an operator-owned location
such as `/etc/lore-youtrack/bridge.toml`. Edit the cache path, database path, Lore
binary path, receiver URL, branches, and author mappings. Configuration, database,
and secret files inside the monitored clone are rejected.

Provide the Auth Header in `LORE_YOUTRACK_SIGNATURE` or use `signature_file` instead
of `signature_env`. The secret is never stored in the SQLite database. A secret
file may end with one newline. Do not place real secrets in this checkout or
commit them. Set optional read-token credentials separately.

For author mappings, read `created-by` from a real remote revision's metadata:

```bash
lore --json --non-interactive --remote --repository /var/lib/lore-youtrack/cache revision info REVISION_HASH
```

Map that exact identity in `[authors."identity"]` to `user_name`, `email`, or both.
The bridge uses `committed-by` only when `created-by` is absent or empty. It never
substitutes the user who pushed or its own service account. Missing mappings stop
discovery without partially recording the scan.

## Start synchronization

Run from this directory, or add it to `PYTHONPATH`:

```bash
cd /opt/lore/contrib/youtrack
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml scan
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml sync
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml status
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml run
```

`scan` saves the currently reachable history without making a YouTrack request.
`sync` scans, reconciles unknown attempts when configured, and sends one bounded
batch. `run` repeats this work. They report JSON counters, revision IDs, and safe
diagnostics. Neither HTTP response bodies nor secret headers are logged.

An initial scan imports existing reachable history. To start with future
revisions instead, run `baseline` **instead of the initial scan** on an empty
database:

```bash
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml baseline
```

Baseline records existing ancestry as `skipped` and sends nothing. Existing
history needs no author mapping in this mode. A baseline cannot replace or reset
an existing delivery ledger.

The sample [systemd service](lore-youtrack.service) runs continuously. Adapt its
paths, provision its dedicated Unix user and writable directories, and prepare
the bare clone under that user's Lore environment. Its `LORE_GLOBAL_PATH` must
match the environment used to authenticate and clone. No service is installed by
the bridge itself.

## Handle delivery outcomes

| State | Meaning | Automatic behavior |
| --- | --- | --- |
| `pending` | Saved; no unresolved request is in progress | Sends when due |
| `in_flight` | Attempt saved before HTTP begins | A process restart converts it to `unknown` |
| `accepted` | The receiver returned a complete HTTP 2xx response | Never sends again |
| `unknown` | The request may have taken effect | Holds the queue; no automatic POST replay |
| `blocked` | Explicit rejection or invalid request | Holds the queue until corrected |
| `reconciled` | REST exposed the expected revision and repository link in each referenced issue | Never sends again |
| `skipped` | Existing history intentionally baselined | Never sends |

Only connection failures **before any HTTP request is attempted** are retried
automatically, with exponential backoff capped at 300 seconds. Response loss,
truncated replies, HTTP 5xx, and 408/425/429 are uncertain outcomes. Redirects are
not followed. An unresolved earlier delivery holds later ones to preserve the
observed order. SQLite commits the ancestry and outgoing records together, and
commits `in_flight` before sending. One process owns the database through an OS
lock; `status` can read a snapshot while it runs without changing state.

After correcting an explicitly rejected request, stop the daemon and use:

```bash
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml retry-blocked REVISION_HASH
```

This command accepts only the listed client-error rejection statuses. It cannot
retry `unknown`. Previously saved payloads retain their original author mapping
and message; editing configuration does not rewrite historical requests.

### Read-only reconciliation

To resolve a lost HTTP response without sending it again, configure
`read_token_env` or `read_token_file`, plus `commit_url_template`. The template
must match the repository URL configured in YouTrack. `project_keys` should list
all projects connected to that receiver, so unrelated text is not mistaken for
a required association. The permanent token needs read access to those issues
and visibility of their VCS changes.

```bash
python3 -m lore_youtrack --config /etc/lore-youtrack/bridge.toml reconcile
```

The bridge reads the paginated `vcsChanges` collection on the same YouTrack
origin and context path. It requires the exact revision signature and expected
repository URL in every referenced issue, ignoring detached changes. Absence,
incomplete pagination, missing visibility, or a failed GET leaves the original
attempt unknown. There is deliberately no `retry-unknown` command.

Reconciliation proves visible **associations**. It does not prove that commit
commands ran, that a particular receiver created the association, or that no
other workflow acted. It never replays commands to fill this gap. If an unknown
request cannot be reconciled, retain the database and investigate the receiver's
logs; deleting the database would discard duplicate protection.

## Limits and operations

| Setting or limit | Default / behavior |
| --- | --- |
| **Polling interval** | `poll_seconds = 30` |
| **Lore subprocess deadline** | `lore_timeout = 60` seconds |
| **HTTP socket timeout** | `request_timeout = 15` seconds; not an overall deadline |
| **New revisions per scan** | `scan_limit = 1000`; raise deliberately for a larger import |
| **Discovery metadata** | 32 MiB per graph walk; 32 MiB of outgoing payloads per scan |
| **Lore command output** | 8 MiB including stderr |
| **Outgoing payload** | 1 MiB per revision in discovery |
| **Deliveries per cycle** | `send_limit = 100` |
| **HTTP response body** | 64 KiB |
| **REST reconciliation** | 100 items per page; at most 100 GET requests per attempt |
| **Database** | Local SQLite WAL with synchronous FULL; one writer, not shared over NFS |

A limit fails discovery instead of silently truncating history. The ledger grows
with revision count and stores commit metadata and messages. Protect and back it
up as repository data. Stop the service before copying the database and its WAL,
or use SQLite's online backup facility. Do not reset it as a retry mechanism.

Use HTTPS for YouTrack; `allow_http_loopback = true` is only for explicit local
tests. TLS uses the platform trust store. Redirects and environment HTTP proxies
are not used. HTTP socket timeouts bound idle socket operations; DNS resolution
and a server that trickles response bytes can take longer. A service manager may
terminate a stuck process: recovery will preserve an uncertain attempt as
`unknown`.

Polling recovers revisions still reachable from selected remote branches and
their stack points. It cannot recover a branch created and deleted entirely
between polls, or revisions made unreachable before observation. Source
notifications alone would not close this durability gap. A full event audit
requires a durable server journal tied to branch updates.

This integration exports revision metadata. It does not implement PRs, review,
build status, merge automation, issue creation, or a Lore web UI. Those need
their own explicit interfaces.

Exit code `0` means the command completed. `scan`, `sync`, and `reconcile` return
`2` when unknown, blocked, or in-flight attempts remain. `sync` also returns `2`
when pending records remain; `reconcile` does not treat pending records as a
failure. Exit code `1` is a configuration, source, or local-state error. `status`
reports state without treating an outstanding delivery as a command failure.
A continuous process logs source failures and tries a later poll.

## Test

From the Lore repository root, run the dependency-free suite:

```bash
PYTHONPATH=contrib/youtrack python3 -m unittest discover -s contrib/youtrack/tests -v
```

The native test is opt-in. Point it at trusted client and server binaries:

```bash
PYTHONPATH=contrib/youtrack \
  LORE_TEST_BINARY=/absolute/path/to/lore \
  LORE_TEST_SERVER_BINARY=/absolute/path/to/loreserver \
  python3 -m unittest discover -s contrib/youtrack/tests -v
```

It creates isolated temporary storage and a loopback-only Lore Server, makes and
merges revisions, uses a bare reader, and runs the actual bridge command against
a local HTTP receiver. It checks the wire payload, restart deduplication,
unpushed-history exclusion, and a receiver that accepts a request but drops its
response. Unit tests cover incomplete CLI output, both DAG parents, resource
limits, crashes, concurrent writers, author mapping, rejected requests,
credential handling, pagination, and positive reconciliation.

These tests prove the Lore-to-bridge-to-HTTP path. They do not substitute for a
qualification run on your real YouTrack instance. See [validation record](VALIDATION.md)
for the versions and commands actually run for this change.
