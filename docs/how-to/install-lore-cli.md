# Install the Lore CLI

`lore` is the CLI for the Lore version control system. It handles the full workflow — from branching, merging, and shipping your work to more advanced tasks like sparse clones and offline commits — all from a single binary.

In this guide, you'll install `lore`, add it to your PATH, and set up shell completions so that you can effectively drive Lore, at full speed, directly from your terminal.

## Prerequisites

- **If building from source:**
  - A [Rust toolchain](https://www.rust-lang.org/tools/install)
  - The Lore repository cloned locally

## Choose an install path

The two paths are mutually exclusive, and each one below is complete on its own — follow one top to bottom, and don't jump between them.

- **[Install the prebuilt binary](#install-the-prebuilt-binary):** Pick this for a normal install from a published release. You don't need a Rust toolchain or a local checkout.
- **[Build from source and install](#build-from-source-and-install):** Pick this to run a CLI built from your own checkout, using a Rust toolchain.

### Install the prebuilt binary

Pick this path for a normal install from a published release.

1. **Run the installer.**

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    curl -fsSL https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.sh | bash
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    irm https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.ps1 | iex
    ```

    Or install with [Scoop](https://scoop.sh):

    ```powershell
    scoop install lore
    ```

    <!-- tabs:end -->

    The installer downloads the binary for your platform and adds it to your PATH. Open a new terminal session for the PATH change to take effect.

### Build from source and install

Pick this path to run a CLI built from your own checkout.

1. **Build the binary.**

    From the Lore repository root:

    ```bash
    cargo build --release -p lore-client --bin lore
    ```

    The compiled binary lands at `target/release/lore` (`target\release\lore.exe` on Windows). The first build compiles from source and may take several minutes.

2. **Put `lore` on your PATH.**

    Move the compiled binary into a directory on your PATH so you can run `lore` from any directory.

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    Using `/usr/local/bin` (no PATH changes needed):

    ```bash
    sudo cp target/release/lore /usr/local/bin/lore
    ```

    Using `~/bin` instead (no `sudo` required):

    ```bash
    mkdir -p ~/bin
    cp target/release/lore ~/bin/lore
    ```

    If you used `~/bin`, add it to your PATH. For zsh (the default on macOS):

    ```bash
    echo 'export PATH="$HOME/bin:$PATH"' >> ~/.zshrc
    source ~/.zshrc
    ```

    For bash:

    ```bash
    echo 'export PATH="$HOME/bin:$PATH"' >> ~/.bashrc
    source ~/.bashrc
    ```

    <!-- tab -->
    **Windows (PowerShell)**

    Copy the executable into a directory you control, for example `%USERPROFILE%\bin`:

    ```powershell
    New-Item -ItemType Directory -Force "$env:USERPROFILE\bin"
    Copy-Item target\release\lore.exe "$env:USERPROFILE\bin\lore.exe"
    ```

    Add that directory to your PATH:

    ```powershell
    [Environment]::SetEnvironmentVariable(
      "Path",
      [Environment]::GetEnvironmentVariable("Path", "User") + ";$env:USERPROFILE\bin",
      "User"
    )
    ```

    Open a new terminal afterward. An existing terminal won't reflect the PATH change.

    <!-- tabs:end -->

## Install shell completions (optional)

The `lore completions` command generates and installs shell completions for `lore`. Once installed, pressing Tab completes commands or lists possible matches when the input is ambiguous. To run it, pass the shell name and an optional output directory; omit the directory to print the script to standard output instead. Lore also supports `fish` and `elvish` through the same command.

<!-- tabs:start -->

<!-- tab -->
**zsh**

```bash
lore completions zsh ~/.zsh/completions
```

Make sure the target directory is on your `fpath`, then restart your shell.

<!-- tab -->
**bash**

```bash
lore completions bash > ~/.local/share/bash-completion/completions/lore
```

Restart your shell to load the completions.

<!-- tab -->
**PowerShell**

```powershell
lore completions powershell | Out-String | Invoke-Expression
```

To make completions persistent, add that line to your PowerShell profile.

<!-- tabs:end -->

## Result

```bash
lore --version
```

Running this from any directory prints a `lore <version>` line. A source build derives its version from the repository checkout, so the exact string depends on the source tree you built from.

## See also

- [Quickstart](../tutorials/quickstart.md) — clone a repository, stage and commit changes, and push your first revision.
- [Lore CLI command reference](../reference/lore-cli-commands.md) — every `lore` command, subcommand, and flag.
- [Deploy a local Lore Server](deploy-local-lore-server.md) — stand up a persistent server to push to and clone from.
- [Lore CLI config reference](../reference/lore-cli-config.md) — every CLI config option and its default.
