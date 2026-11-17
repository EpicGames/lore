# Set up, build and test Lore

How to set up a development environment for Lore, build it, run its tests and checks, and sign off commits. What a contribution must meet to be accepted is in [`CONTRIBUTING.md`](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md).

## Install prerequisites

- **Rust**: install with [rustup](https://rustup.rs/). Lore needs the stable toolchain, and nightly for formatting.
- **Python 3.13 or 3.14**: needed for the smoke tests, with [uv](https://github.com/astral-sh/uv) for dependencies.

Install the Rust toolchains:

```sh
rustup toolchain install stable
rustup toolchain install nightly
```

Optionally, install [pre-commit](https://pre-commit.com/) to run the format and lint checks on each commit. CI runs the same checks, so this is a convenience:

```sh
uv tool install pre-commit
pre-commit install
```

## Build

```sh
cargo build
```

`cargo build -p lore-capi` builds the C library, `liblore.so` and `liblore.a`, and its header `lore.h`. Building only the `lore` crate, the CLI or the server doesn't build it.

### Stamp a build version

A build reports its version as `<package version>+local`. No build compiles a per-build version in, so a changing build number never invalidates the build cache. To give finished artifacts a build version, stamp them after building and before signing:

```sh
cargo run --release -p lore-base --bin lore-stamp -- --build <name> \
    target/release/lore target/release/loreserver \
    target/release/liblore.so target/release/liblore.a
```

Each binary and library then reports `<package version>+<name>`. `lore-stamp` writes only the name, so each file reports the package version it was built with. `scripts/stamp-artifacts.sh` stamps the number of the revision the working tree is at, into the files it's given or, given none, into every artifact of a full release build. The build name is limited to letters, digits, and ``!#$%&'*+-.^_`|~``. `lore-stamp` writes no file unless every file holds exactly one version slot, and a stamped file can be stamped again. On macOS the library is `liblore.dylib`; on Windows the files are `lore.exe`, `loreserver.exe`, `lore.dll`, and `lore.lib`. Stamp macOS artifacts on macOS, where `lore-stamp` signs ad-hoc again each file the linker signed ad-hoc and refuses a file signed with an identity.

## Run the tests

Run the unit and integration tests, and the smoke tests against a release build:

```sh
cargo test --release
cargo build --release -p lore-client -p lore-server --bin lore --bin loreserver
uv run pytest scripts/test/ --lore-client-binary=release --lore-server-binary=release
```

The SWFS smoke tests need the EpicVFS driver, which isn't publicly available. Without it, skip them:

```sh
uv run pytest scripts/test/ -m "smoke and not swfs"
```

[`testing.md`](code-standards/testing.md) describes what a change must test and the other ways to run the tests.

## Run the format and lint checks

CI runs these checks, and they must pass before a pull request merges:

```sh
cargo +nightly fmt --all
cargo clippy --all-targets -- -D warnings --no-deps
```

For a change to docs, run the three doc linters. The [doc-standards tooling guide](doc-standards/tools/README.md) covers installing and configuring them:

```sh
vale <file>                   # Lore house style
npx markdownlint-cli2 <file>  # Markdown formatting
lychee --config docs/developing/doc-standards/tools/lychee/lychee.toml docs/
# Run lychee across the whole tree, not only changed files: a moved or
# renamed page can break links in docs you didn't touch.
```

## Sign off commits

Every commit carries a `Signed-off-by:` line, as [`CONTRIBUTING.md`](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md#dco-sign-off) requires. Add it with `-s`:

```sh
git commit -s -m "Your commit message"
```

This appends:

```text
Signed-off-by: Your Name <you@example.com>
```

## See also

- [`testing.md`](code-standards/testing.md): what a change must test, and other ways to run the tests.
- [`CONTRIBUTING.md`](https://github.com/EpicGames/lore/blob/main/CONTRIBUTING.md): what a contribution must meet to be accepted.
