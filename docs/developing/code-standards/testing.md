# Lore testing standards

This document defines the standard patterns for testing across the Lore codebase.

## Overview

| Type | Location | Framework | Purpose |
| --- | --- | --- | --- |
| **Unit tests** | `tests/unit/` in each crate | Rust/tokio | Module-level testing |
| **Integration tests** | `lore-revision/tests/` | Rust/tokio | Cross-module testing |
| **Smoke tests** | `scripts/test/` | Python/pytest | CLI and server testing |
| **Load tests** | Internal infrastructure | Internal harness | Performance testing |

---

## 1. Target risk at the cheapest tier that can find the fault

Test effort follows risk, not line count. Business logic — session state, ordering guarantees, allocation and placement decisions — earns thorough tests. Plumbing and trivial code earn few or none. A test that restates the implementation costs maintenance and finds nothing.

Then split by what you are checking:

- **Edge cases, variants, boundaries, and error paths → unit tests.** They are cheap, so be exhaustive.
- **Happy paths → tests that run against real things.** Mocks cannot tell you whether this works wired to a real dependency or assembled into the binary we ship, and it is not worth asking twenty times over.

Assert on outcomes, not on wording. Check `Ok` or `Err` and the error variant, never a substring of the message. Message text is presentation: it changes for readability and the test fails for no reason, or it stops matching a real regression and passes for no reason.

### The tiers

Cheapest first. Cost tracks how much must be standing before the test can run.

| Tier | Answers | Location | Needs |
| --- | --- | --- | --- |
| Unit | Is this logic correct across all its cases? | `tests/unit/` | Nothing |
| Smoke | Does a real user flow work across the binaries? | `scripts/test/`, `@pytest.mark.smoke` | `lore` and `loreserver` binaries built |

Edge cases belong in unit tests. The smoke test covers the happy path only.

---

## 2. Rust unit tests

Unit tests live in the crate's `tests/unit/` directory, not in inline `#[cfg(test)]` modules. Cargo builds the directory as one test binary, which links the library like any other user of it:

```text
lore-telemetry/
├── Cargo.toml              # [lib] test = false
├── src/
│   └── observe.rs
└── tests/
    └── unit/
        ├── main.rs         # mod observe; mod user_agent_filter;
        ├── observe.rs
        └── user_agent_filter.rs
```

```rust
// tests/unit/observe.rs
use lore_telemetry::observe::ObserveResult;

#[tokio::test]
async fn can_observe_success_no_callback() {
    // test code
}
```

An inline `#[cfg(test)]` module makes cargo compile the whole crate a second time, with `--test`. That compile produces an executable, which the build cache cannot store, so CI repeats it on every run. Tests in `tests/unit/` use the library build every other crate already shares, and the build cache serves it.

Set `[lib] test = false` in the crate's `Cargo.toml` once no inline test is left. Without it cargo still compiles the crate a second time, as an empty test harness. The setting covers the whole crate, so move all of a crate's inline modules together. Crates that still carry inline modules move them when next worked on.

### One binary, unless a test needs its own process

All tests in `tests/unit/` share one process, as the tests of an inline module do. A test that needs a process of its own goes in a separate `tests/<name>.rs` file, which cargo builds as its own binary: one that changes process-wide state other tests would observe, or that exercises process-level behavior such as the library's shutdown. Keep these few. Every file directly in `tests/` is another binary to compile and link, and none of that is cached.

### Test helpers

Put helpers that only the crate's own tests use in `tests/unit/` as well, as modules beside the tests: mocks, factories, fixtures. Their dependencies go in `[dev-dependencies]`. A `mockall::mock!` of a public trait works there as it does inside the crate.

Helpers that other crates' tests use are the exception. Another crate cannot import a module from `tests/`, so these live in the library behind the crate's `test-util` feature, as `lore_base::test_util::TempDir` does (see [Scratch space on disk](#scratch-space-on-disk)).

So is a test double that library code has to name itself, such as the type behind a variant of a closed enum. It lives in the library behind `test-util` as well, as `lore_revision::fs::filesystem_provider::test_util::TestOperation` does, and the rest of the fixture stays in `tests/unit/`.

### When a test needs something private

Look for the public way first: a trait method, a constructor, a re-export. A test that needs private access often checks an implementation detail.

When a test does need an item that is not public, make it public for tests only, behind the crate's `test-util` feature, and enable the feature from the crate's own dev-dependencies:

```toml
[dependencies]
lore-macro = { workspace = true }

[features]
test-util = []

[dev-dependencies]
lore-foo = { workspace = true, features = ["test-util"] }
```

Mark the item with `#[lore_macro::test_pub]`. Built with `test-util`, the item is `pub`, and so are a struct's fields. Built without it, the item is exactly as written. It applies to functions and methods, structs, enums, constants, `static` items, type aliases, and traits. Put it first among the item's attributes, above any `#[derive]`:

```rust
// lore-storage/src/local/immutable_store/info.rs
#[lore_macro::test_pub]
const INFO_MAGIC: u32 = u32::from_le_bytes(*b"IS_I");

#[lore_macro::test_pub]
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable)]
pub struct ImmutableStoreInfo {
    magic: u32,
    version: u32,
    pub next_group_index_to_migrate_oodle: i32,
}
```

When a test needs read access or a wrapper rather than a wider item, add one gated `test_util` child module beside the private items instead. A child module sees its parent's private items, and an inherent `impl` may live anywhere in the crate, so accessors, constructors, and wrappers all fit in it and the production code stays as it is:

```rust
// lore-credential/src/token_store.rs
#[cfg(feature = "test-util")]
pub mod test_util {
    use super::IdentityToken;
    use super::TokenStoreError;

    impl IdentityToken {
        pub fn user_id(&self) -> &str {
            &self.user_id
        }
    }

    pub fn seal_token(key: &[u8], user_token: &str) -> Result<String, TokenStoreError> {
        super::seal_token(key, user_token)
    }
}
```

To make a private module public, switch its declaration. The attribute cannot do this, because it does not apply to a module declared in its own file. A module that already has a `cfg` keeps it in both declarations:

```rust
#[cfg(not(feature = "test-util"))]
mod internals;
#[cfg(feature = "test-util")]
pub mod internals;

#[cfg(all(feature = "oodle", not(feature = "test-util")))]
mod oodle_migration;
#[cfg(all(feature = "oodle", feature = "test-util"))]
pub mod oodle_migration;
```

`cfg(test)` is set only when cargo compiles the crate itself as a test. The tests in `tests/unit/` link the library as built normally, so a `#[cfg(test)]` item or a `#[cfg_attr(test, ...)]` in library code is not there for them. When moving inline tests out, change each one to `feature = "test-util"`.

Used this way the feature adds access and nothing else: wider visibility, accessors, constructors, and wrappers over code that already exists. Helper code stays in `tests/unit/` and its dependencies in `[dev-dependencies]`. Release builds never see it: cargo applies a dev-dependency's features only to builds that include the dev-dependencies. Do not make an item `pub` unconditionally for a test. It becomes API other crates can reach, and the compiler stops reporting it once it falls out of use.

---

## 3. Rust async tests

**Frameworks:** `tokio`, `mockall`, `async-trait`

All async tests use the `LORE_CONTEXT.scope()` pattern:

```rust
#[tokio::test]
async fn test_example() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            // test code
        })
        .await;
}
```

### Test independence

Tests MUST be independent and isolated. Avoid:

- **`#[serial]`** — Forces sequential execution, indicating shared mutable state.
- **Test dependencies** — Tests that rely on other tests running first.

If you find yourself needing `#[serial]`, refactor the test to:

1. Create isolated test fixtures and state per test.
2. Use unique identifiers (for example, random repository names or unique temp directories).
3. Mock shared resources instead of using real shared state.

```rust
// Anti-pattern: shared state requiring serial execution
#[tokio::test]
#[serial]  // DON'T DO THIS
async fn test_with_shared_state() { ... }

// Preferred: isolated test with unique fixtures
#[tokio::test]
async fn test_with_isolated_state() {
    let test_repo = test_store_create();  // Creates unique test repository
    // Test uses only this isolated state
}
```

**Key files:**

- `lore-revision/tests/helper.rs` — `test_store_create()`, `setup_test_execution()`.
- `lore-revision/tests/` — Cross-module integration tests.
- `lore-integration-tests/` — Tests against real infrastructure, without mocking

### Scratch space on disk

`lore_base::test_util::TempDir`, and `TempFile` for a single file, are the only
way a test asks for scratch space on disk. Both are behind lore-base's
`test-util` feature, which the crates that need it enable from their
dev-dependencies:

```toml
[dev-dependencies]
lore-base = { workspace = true, features = ["test-util"] }
```

```rust
let dir = TempDir::new("my-test-");       // <temp>/my-test-A1b2C3d4
std::fs::write(dir.child("data.bin"), b"...").expect("write");

let file = TempFile::with_contents("my-test-", b"payload");
some_api(file.path());
```

The name carries a random suffix, so two tests — or two runs on one machine —
never share a directory, and removal happens in `Drop`, so it happens whether
the test passes or fails an assertion.

**Do not** build a path under `std::env::temp_dir()` by hand, and **do not**
remove a directory on the last line of a test body: a failing assertion panics
before reaching that line, so the run that most needs its output is the one that
leaks it.

**Return the guard, never a path derived from it.** A helper that ends
`temp.path().to_path_buf()` drops the guard on return and deletes the directory
its caller is about to read. Hand back the `TempDir` and let the caller hold it:

```rust
fn fixture() -> (TempDir, PathBuf) { ... }   // correct
fn fixture() -> PathBuf { ... }              // deletes the directory on return
```

Set `LORE_KEEP_TEST_DATA=1` to keep the directories rather than remove them —
the Rust equivalent of the smoke tests' `--keep-test-data`. The prefix is all
there is to identify one afterwards, so give it the test's name wherever several
tests in a module would otherwise share it.

`tempfile` is the implementation and a dependency of `lore-base` only. Do not add
it to another crate: a second guard type is a second set of rules, and it does not
honour `LORE_KEEP_TEST_DATA`.

### Polling for async state

Poll for a condition instead of sleeping. Fixed sleeps are flaky on slow CI, wasteful on fast machines, and opaque — a failure does not say whether the timeout was wrong or the code is broken.

```rust
// BAD
sleep(Duration::from_millis(500)).await;
// GOOD
wait_for_condition(|| async { manager.session_count() == 2 }, Duration::from_secs(5))
    .await
    .expect("sessions should appear within 5 seconds");
```

Fixed sleeps are acceptable only for time-based behavior such as TTL expiry, or pauses under 20ms to let a spawned task start.

---

## 4. Smoke tests (`scripts/test/`)

**Framework:** pytest

**Requirement:** All Lore CLI commands must have smoke test coverage. When adding a new command, add corresponding tests to `scripts/test/`.

### Key files

| File | Purpose |
| --- | --- |
| `conftest.py` | Fixtures and server management |
| `lore.py` | `Lore` wrapper class for the CLI |
| `error_types.py` | Exception mapping from CLI output |

### Fixtures

- `new_lore_repo` — Creates a new test repository.
- `scratch_dir` — Hands out a path beside the repositories for the test to create
  something at: a shared store, a clone target, a moved instance.
- `lore_executable_path` — Path to the Lore client binary.
- `auto_lore_local_server` — Auto starts the server for the session.

### Test data is removed as the run goes

A full run writes about ten gigabytes, so nothing is kept once it is no longer
needed. Cleanup is in two layers:

1. **Per test.** `new_lore_repo` removes every repository it handed out and
   every repository those went on to clone; `scratch_dir` removes every path it
   handed out; `global_dir_name` removes the isolated global config and the
   shared stores under it. All of this runs whether the test passed or failed.
   A server built inside a test or a class fixture through
   `generate_server_config` is removed at that same scope, after the fixture
   that launched it has stopped it. An autouse fixture removes the per-test
   `tmp_path`, which pytest itself only removes under the `failed` policy and
   then only for tests that passed.
2. **Per session.** `_SessionCleanup` in `lore_server.py` empties basetemp once
   every worker has finished, taking the session server's root and anything a
   per-test cleanup could not.

**A test that creates files outside its repository must take `scratch_dir` and
create them at a path it hands out.** Anything written straight to
`tmp_path_factory.getbasetemp()` survives the test that made it and is only
caught by the session sweep, which is a backstop, not the guard.

Removal is best effort and never fails a test: a locked file is logged and left
to the sweep.

Pass `--keep-test-data` to keep everything on disk — repositories, stores,
server roots and the server log — when investigating a failure.

### Usage

```python
@pytest.mark.smoke
def test_commit(new_lore_repo):
    repo: Lore = new_lore_repo()
    repo.stage(offline=True)
    repo.commit("Test commit", offline=True)
    repo.push()
```

### Running with uv

Tests require Python 3.13+. Use `uv` to manage dependencies and run tests:

```bash
# Install dependencies
uv sync

# Run all smoke tests with local server
uv run pytest scripts/test/ --lore-client-binary=release --lore-server-binary=release

# Run only tests marked as smoke
uv run pytest scripts/test/ -m smoke

# Run tests in parallel (uses pytest-xdist)
uv run pytest scripts/test/ -n auto

# Against external server
uv run pytest scripts/test/ --disable-local-server --lore-remote-url=lore://host:port
```

### Command-line options

| Option | Default | Description |
| --- | --- | --- |
| `--lore-client-binary` | `release` | Path or "release"/"debug" |
| `--lore-server-binary` | `release` | Path or "release"/"debug" |
| `--lore-remote-url` | `lore://127.0.0.1:41338` | Server address |
| `--disable-local-server` | `false` | Use external server |
| `--disable-auto-server` | `false` | Don't auto start the server |
| `--keep-test-data` | `false` | Leave repositories, stores and server roots on disk |

### Running every test through one service

`LORE_TEST_SHARED_SERVICE=1` starts one service from the build under test before
the run and stops it when the run ends. Every command of every test then runs in
that service, except in the tests that manage a service of their own through
`lore_service_runner`, which keep theirs, and in the tests marked
`runs_in_process(reason)`, whose commands stay in their own process. No
executable is named for the relayed commands, so a service that stops partway
fails the tests that follow instead of being replaced.

```bash
LORE_TEST_SHARED_SERVICE=1 uv run pytest scripts/test/ -m smoke -n 4
```

The service listens on a socket named for the run, with a global directory of
its own, so several runs proceed side by side. A run ended by Ctrl+C, SIGTERM or
SIGHUP stops it as a finished run does. On Linux the kernel stops it when the run
is killed outright, as it does every service and server a test or an xdist
worker starts when that process ends. The server the workers share outlives the
worker that launched it, which can finish first: the run's cleanups stop it, and
on Linux so does the run's main process ending.

`LORE_TEST_SERVICE_SOCKET` names the socket of a service started some other way,
which the run then uses in place of starting one:

```bash
LORE_SERVICE_SOCKET=lore_service-smoke LORE_GLOBAL_PATH=$(mktemp -d) \
    target/release/lore service run &
LORE_TEST_SERVICE_SOCKET=lore_service-smoke uv run pytest scripts/test/ -m smoke -n 4
```

Each call carries the test's `LORE_GLOBAL_PATH` and `LORE_AUTH_PATH`, which the
service reads in place of its own, so the tests stay isolated from each other.

### Per-test timeout

Every test is bounded at ten minutes (`timeout` in `pyproject.toml`), and the
stack of every thread is dumped a minute before that (`faulthandler_timeout`).
Nothing else bounds a test: the helper runs the client with `subprocess.run` and
no timeout, so a client that never returns would otherwise be a run that never
returns.

A test that reaches the limit is reported as a failure and the run continues. On
Linux the test fails in place; on Windows there is no `SIGALRM`, so the worker is
killed and `xdist` reports
`worker 'gwN' crashed while running <test>` and replaces it. Either way the log
names the test and carries the stack it was stuck in.

Override with `--timeout=0` to disable, or `--timeout=<seconds>` for a run that
is legitimately slower.

### pytest markers

| Marker | Description |
| --- | --- |
| `@pytest.mark.smoke` | Smoke tests for basic functionality |
| `@pytest.mark.slow` | Slow running tests |
| `@pytest.mark.disable_auto_server` | Tests requiring the `--disable-auto-server` flag |

---

## 5. Load tests

Lore has a load-testing suite that exercises concurrent clone, commit, sync, lock, and compaction workloads. It runs on internal infrastructure and isn't part of the open-source repository, so its harness and scenarios aren't documented here.

---

## 6. Best practices

1. **Target risk, not line count** — spend test effort on business logic; leave plumbing and trivial code with few or no tests.
2. **All Lore commands must have smoke tests** in `scripts/test/`.
3. **Use `LORE_CONTEXT.scope()`** for all async Rust tests.
4. **Keep tests independent** — Avoid `#[serial]` and test dependencies; use isolated fixtures.
5. **Poll instead of sleeping** for async state; reserve fixed sleeps for time-based behavior or sub-20ms task startup.
6. **Use the `new_lore_repo` fixture** for smoke tests, and `scratch_dir` for anything
   a test creates outside its repository — both remove what they hand out.
7. **Mark tests** with `@pytest.mark.smoke` for smoke test runs.
8. **Use `offline=True`** for operations that don't need the server.
9. **Feature-gate integration tests** that require external dependencies.
10. **Put Rust unit tests in `tests/unit/`**, with `[lib] test = false`. Give a test its own `tests/*.rs` binary only when it needs its own process, and reach private items only through the `test-util` feature, usually with `#[lore_macro::test_pub]`.
