---
name: smoke-tests
description: Build the project with Cargo and run smoke tests with pytest. If build or tests fail, proposes a plan to fix the issues.
argument-hint: [workers=N] [debug|release] [test filter]
---

# Smoke Tests

Run the project's smoke test suite, handling any build or test failures.

## Arguments

The skill accepts optional arguments: a worker count for parallel execution, a build mode (`debug` or `release`), and/or a test filter (e.g., `/smoke-tests`, `/smoke-tests workers=9`, `/smoke-tests workers=9 test_replication`, `/smoke-tests debug test_replication`).

Ports for `loreserver` are allocated dynamically by the test harness (free ephemeral ports asked from the OS), so concurrent invocations of this skill don't collide and there's no per-session port bookkeeping to do.

### Workers (optional, `workers=N`)

An argument matching `workers=N` enables pytest-xdist parallel execution with N workers by adding `-n N` to the pytest command. If omitted, no `-n` flag is added and tests run sequentially.

All workers in a run share a single `loreserver` launched by `gw0`; secondary worker fixtures used in `test_replicated_store` and `test_topology` allocate their own free ephemeral ports dynamically. Pick whatever worker count fits the host — there's no per-session cap.

### Build Mode (optional, `debug` or `release`)

Pass the literal token `debug` or `release` to set the Cargo build profile. If omitted, the mode is `release`, for a filtered run and a full run alike, as `docs/developing/code-standards/testing.md` describes in *Build profile*: the release profile compiles incrementally, and one build serves every test run.

### Test Filter (optional)

Any argument that isn't `workers=N` or `debug` / `release` is treated as a test filter and passed to pytest via `-k`. This uses pytest's keyword expression matching, which supports:

- **Module name**: `test_replication` — runs all tests in files/classes matching that name
- **Test function name**: `test_basic_put` — runs tests matching that name
- **Compound expressions**: `test_replication and test_basic` — combines filters with `and`, `or`, `not`

Examples:
- `/smoke-tests` — all smoke tests, sequential, **release build** (no filter)
- `/smoke-tests workers=9` — all smoke tests, 9 parallel workers, **release build** (no filter)
- `/smoke-tests test_replication` — replication tests, sequential, **release build**
- `/smoke-tests workers=4 test_replication` — replication tests, 4 workers, **release build**
- `/smoke-tests test_basic_put` — `test_basic_put`, **release build**
- `/smoke-tests workers=9 "test_replication and not test_slow"` — compound filter, 9 workers, **release build**
- `/smoke-tests debug test_replication` — replication tests with **debug build** (override; rarely useful)

## Instructions

### Step 1: Parse Arguments

Split the provided arguments on whitespace:

1. **Arguments matching `workers=N`** set the xdist worker count. If none is given, no `-n` flag is added (sequential execution).
2. **Arguments matching `debug` or `release`** set the build mode explicitly.
3. **All other arguments** are treated as a pytest `-k` test filter. If none is given, no `-k` flag is added (all smoke tests run).

Then resolve `BUILD_MODE`:

- If `debug` or `release` was passed explicitly, use it.
- Otherwise, use `release`.

### Step 2: Build the Project

Run the Cargo build for the resolved `BUILD_MODE`:

```bash
# release
cargo build --release -p lore-client -p lore-server --bin lore --bin loreserver

# debug
cargo build -p lore-client -p lore-server --bin lore --bin loreserver
```

If the build fails:
- Analyze the compiler errors
- Use EnterPlanMode to propose fixes
- After plan approval, implement the fixes and rebuild

### Step 3: Run Smoke Tests

Once the build succeeds, run the smoke tests. Optionally append `-n ${WORKERS}` if workers was specified, and `-k "${TEST_FILTER}"` if a test filter was provided. Do not pass `--lore-remote-*-port` or `--lore-remote-url` — the harness allocates ports dynamically.

Use the binaries from `./target/${BUILD_MODE}/` (i.e. `./target/release/` or `./target/debug/`).

**Base command (sequential, all smoke tests):**
```bash
uv run pytest scripts/test -m=smoke --log-cli-level=INFO \
  --lore-server-binary=./target/${BUILD_MODE}/loreserver \
  --lore-client-binary=./target/${BUILD_MODE}/lore
```

**With workers (parallel execution):** append `-n ${WORKERS}`

**With test filter:** append `-k "${TEST_FILTER}"`

Example: no filter, no workers:
```bash
uv run pytest scripts/test -m=smoke --log-cli-level=INFO \
  --lore-server-binary=./target/release/loreserver \
  --lore-client-binary=./target/release/lore
```

Example: 4 workers, filter `test_replication`:
```bash
uv run pytest scripts/test -m=smoke --log-cli-level=INFO \
  --lore-server-binary=./target/release/loreserver \
  --lore-client-binary=./target/release/lore \
  -n 4 \
  -k "test_replication"
```

### Step 4: Handle Failures

If either build or tests fail:

1. Analyze the error output to understand the root cause
2. Use EnterPlanMode to propose a fix plan
3. After user approval, implement the fixes
4. Re-run the failed step to verify the fix
5. Continue until all tests pass

## Notes

- Always build before running tests to ensure binaries are up-to-date
- The smoke tests require both `loreserver` and `lore` binaries from `./target/${BUILD_MODE}/`
- **The default build mode is `release`**, with or without a filter, as `testing.md` describes. Pass `debug` to override it.
- Read relevant code standards from `docs/developing/code-standards/` when fixing issues
- **Use a 20-minute timeout** for both build and test commands as they can take a while
- **Do not use `head` or `tail`** to view build or test output. Instead, redirect output to a timestamped temp file (e.g., `cargo build --release -p lore-client -p lore-server --bin lore --bin loreserver 2>&1 | tee /tmp/smoke-build-20260303-1425.log`) and use the Read tool to inspect the results. The timestamp disambiguates concurrent runs.
