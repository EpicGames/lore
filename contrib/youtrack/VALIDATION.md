# YouTrack bridge validation

Validation date: October 31, 2026.

## Tested code and tools

| Item | Version or revision |
| --- | --- |
| **Upstream source baseline** | `b8fc623d5b064e910012957c58dc6d3597c45b41`, workspace `0.12.1-nightly` |
| **Bridge** | `0.1.0`, source and tests in this directory |
| **Native client and server** | Official Lore `v0.12.0`, client reports `0.12.0+1967` |
| **Official release commit** | `f00dbbf07cb449103e211ccbecaa995630f47d2d` |
| **Platform** | Linux x86_64 |
| **Python** | 3.12.14 |
| **Ruff** | 0.17.3 |
| **YouTrack protocol reference** | Server / Cloud 2026.2 Generic VCS documentation |

Official release archives were verified against the SHA256 values published
by the GitHub release API before extraction:

| Archive | SHA256 |
| --- | --- |
| `lore-v0.12.0-x86_64-unknown-linux-gnu.tar.gz` | `27c312c4e3ddce160bbbce7356fba5698fdecead049bf4b4c704174c3d9a8f28` |
| `loreserver-v0.12.0-x86_64-unknown-linux-gnu.tar.gz` | `9372eb024ff571063b8b783dc99281fed7d73c96e234770266736873443d773a` |

Source: [official v0.12.0 release](https://github.com/EpicGames/lore/releases/tag/v0.12.0),
published October 27, 2026.

## Results

| Gate | Observed result |
| --- | --- |
| **Full bridge suite** | 60 tests passed, 0 skipped, 2.705 seconds |
| **Python formatting** | `ruff format` completed; final check passes |
| **Python lint** | `ruff check` passed with the repository's configuration |
| **Whitespace** | `git diff --check` passed |
| **Independent source review** | Reviewed delivery, recovery, DAG, binding, credentials, and docs; no confirmed P1/P2 defects remained |
| **Real YouTrack deployment** | Not run; no instance URL or credentials were supplied |
| **Rust workspace rebuild** | Not run; no Rust code, wire formats, or dependencies changed |
| **Other operating systems / Python versions** | Not run |

The full suite was run from the repository root with the downloaded native
binaries selected through `LORE_TEST_BINARY` and `LORE_TEST_SERVER_BINARY`:

```bash
PYTHONPATH=contrib/youtrack \
  LORE_TEST_BINARY=/absolute/path/to/lore \
  LORE_TEST_SERVER_BINARY=/absolute/path/to/loreserver \
  python -m unittest discover -s contrib/youtrack/tests -v
```

Observed terminal result:

```text
Ran 60 tests in 2.705s

OK
```

## Native end-to-end scenario

`NativeSourceTests.test_remote_merge_history_and_durable_delivery_from_a_bare_clone`
starts an isolated, real Lore Server bound to loopback. It creates base, feature,
main, and merge revisions, pushes them, and clones a separate bare reader. It
then runs the actual `python -m lore_youtrack ... sync` command as a subprocess.

The receiver is a real local HTTP server implementing the documented Generic
VCS request surface. Assertions check each revision signature, original author
mapping, original millisecond timestamp, message, branch, and literal signature
header. The SQLite ledger survives the bridge process exiting.

A second bridge process discovers no new revisions and sends no new POSTs.
An unpushed local revision is excluded. After that revision is pushed, the
receiver records the next POST and closes the connection without replying.
The bridge records `unknown`; another bridge process sends no duplicate POST.
The bare reader has no checked-out asset files.

## Failure and boundary coverage

Tests also cover both merge parents, branch stack roots, a deep nonrecursive
history, incomplete metadata, mismatched repository and revision IDs, truncated
CLI output, source deadlines and output limits, missing author mappings,
transaction rollback, initial baseline, concurrent-writer exclusion, restart
recovery, read-only status during a live attempt, explicit rejection, response
truncation, 5xx/429, redirect refusal, and bounded REST pagination.

Positive reconciliation requires the exact revision and repository URL in every
referenced issue. Missing, detached, wrong-repository, or incomplete results do
not authorize a retry. Tests verify that sender credentials are absent from the
database and diagnostics.

These results establish the Lore-to-bridge-to-HTTP path and the tested failure
semantics. They do not establish actual YouTrack issue processing, commit-command
effects, authenticated production Lore configuration, or production readiness
of the upstream Lore release.
