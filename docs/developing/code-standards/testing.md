# Lore testing standards

This document defines the standard patterns for testing across the Lore codebase.

## Overview

| Type | Location | Framework | Purpose |
| --- | --- | --- | --- |
| **Unit tests** | Inline `#[cfg(test)]` modules | Rust/tokio | Module-level testing |
| **Integration tests** | `lore-revision/tests/` | Rust/tokio | Cross-module testing |
| **Smoke tests** | `scripts/test/` | Python/pytest | CLI and server testing |
| **Load tests** | Internal infrastructure | Internal harness | Performance testing |

---

## 1. Rust unit tests

Inline in source modules with `#[cfg(test)]`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_example() {
        // synchronous test
    }
}
```

---

## 2. Rust async tests

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

---

## 3. Smoke tests (`scripts/test/`)

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
- `lore_executable_path` — Path to the Lore client binary.
- `auto_lore_local_server` — Auto starts the server for the session.

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

## 4. Load tests

Lore has a load-testing suite that exercises concurrent clone, commit, sync, lock, and compaction workloads. It runs on internal infrastructure and isn't part of the open-source repository, so its harness and scenarios aren't documented here.

---

## 5. Best practices

1. **All Lore commands must have smoke tests** in `scripts/test/`.
2. **Use `LORE_CONTEXT.scope()`** for all async Rust tests.
3. **Keep tests independent** — Avoid `#[serial]` and test dependencies; use isolated fixtures.
4. **Use the `new_lore_repo` fixture** for smoke tests (handles cleanup).
5. **Mark tests** with `@pytest.mark.smoke` for smoke test runs.
6. **Use `offline=True`** for operations that don't need the server.
7. **Feature-gate integration tests** that require external dependencies.
