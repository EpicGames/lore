#!/usr/bin/env bash
# Install the Lore CLI (and, with --demo, a local loreserver) from GitHub Releases.
#
# Quick start:
#   curl -fsSL https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.sh | bash -s -- --demo
#
# On Windows, use the PowerShell peer scripts/install.ps1.
# For flags and their env-var equivalents, run with --help.

set -euo pipefail

REPO="${LORE_REPO:-EpicGames/lore}"
VERSION="${LORE_VERSION:-latest}"
INSTALL_DIR="${LORE_INSTALL_DIR:-$HOME/.local/bin}"
TOKEN="${GITHUB_TOKEN:-}"
GRPC_PORT=41337
HTTP_PORT=41339
DEMO=0
case "${LORE_DEMO:-0}" in 1|true|yes|on|enabled) DEMO=1 ;; esac
SERVER=0
case "${LORE_SERVER:-0}" in 1|true|yes|on|enabled) SERVER=1 ;; esac

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

usage() {
    cat >&2 <<'EOF'
Install the Lore CLI (and, with --demo, a local loreserver) from GitHub Releases.

Usage: install.sh [--demo] [--server] [--version <v>] [--install-dir <dir>] [--repo <owner/repo>] [--token <t>]

Every flag has an env-var equivalent; the flag wins when both are set:

  --demo               LORE_DEMO          also install and launch a local loreserver (1/true/yes/on/enabled)
  --server             LORE_SERVER        only install loreserver (skip the lore CLI and auto-launch)
  --version <v>        LORE_VERSION       install a specific release tag (default: latest)
  --install-dir <dir>  LORE_INSTALL_DIR   where binaries go (default: ~/.local/bin)
  --repo <owner/repo>  LORE_REPO          source repository (default: EpicGames/lore)
  --token <t>          GITHUB_TOKEN       token for private repos / higher rate limit (defaults to `gh auth token`)
  -h, --help                              show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --demo) DEMO=1 ;;
        --server) SERVER=1 ;;
        --version) VERSION="${2:?--version needs a value}"; shift ;;
        --install-dir) INSTALL_DIR="${2:?--install-dir needs a value}"; shift ;;
        --repo) REPO="${2:?--repo needs a value}"; shift ;;
        --token) TOKEN="${2:?--token needs a value}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

for tool in curl tar; do
    command -v "$tool" >/dev/null || die "$tool is required but not installed"
done

# Fall back to the gh CLI's token when none was supplied — installing from a
# private repo then just needs an existing `gh auth login`, not a hand-made PAT.
# Pin github.com: we only ever call api.github.com, and gh may be active on a
# different host (e.g. an enterprise GHE), whose token would not authenticate.
if [[ -z "$TOKEN" ]] && command -v gh >/dev/null; then
    TOKEN="$(gh auth token --hostname github.com 2>/dev/null || true)"
    [[ -n "$TOKEN" ]] && say "using GitHub token from gh CLI"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) die "unsupported OS $(uname -s); on Windows, use scripts/install.ps1 (see docs/how-to/install-lore-cli.md)" ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64) arch=x86_64 ;;
    *) die "unsupported architecture $(uname -m)" ;;
esac
TRIPLE="$arch-$os"

# GET a GitHub URL, using the token when present to lift the API rate limit.
gh_get() { curl -fsSL ${TOKEN:+--oauth2-bearer "$TOKEN"} "$@"; }

# Fetch the release metadata JSON once (latest, or the requested tag).
fetch_release() {
    local api="https://api.github.com/repos/$REPO/releases"
    if [[ "$VERSION" == latest ]]; then api+="/latest"; else api+="/tags/$VERSION"; fi
    gh_get "$api"
}

# Print the API asset URL for <binary>'s tarball matching this platform. We resolve
# the asset's api.github.com URL (not its browser_download_url) so a private-repo
# asset can download with a token via Accept: application/octet-stream. RS=","
# splits the JSON on fields (whitespace-independent) and relies on GitHub
# emitting each asset's "url" before its "name". The triple must be followed
# immediately by ".tar.gz" — we match exact triples only, so a micro-arch variant
# (e.g. ...-aarch64-unknown-linux-gnu.neoverse-512tvb.tar.gz) is deliberately skipped.
asset_url() {
    awk -v bin="$1" -v triple="$TRIPLE" '
        BEGIN { RS = "," }
        /"url"[[:space:]]*:[[:space:]]*"https:\/\/api\.github\.com\/[^"]*\/releases\/assets\/[0-9]+"/ {
            u = $0; sub(/.*"url"[[:space:]]*:[[:space:]]*"/, "", u); sub(/".*/, "", u)
        }
        $0 ~ ("\"name\"[[:space:]]*:[[:space:]]*\"" bin "-v?[0-9][^\"]*-" triple "\\.tar\\.gz\"") { print u; exit }
    ' <<<"$RELEASE_JSON"
}

# Download, unpack, and install <binary> into $INSTALL_DIR, replacing any existing copy.
install_binary() {
    local binary="$1" url
    local bin_path="$INSTALL_DIR/$binary"
    url="$(asset_url "$binary")" || true
    [[ -n "$url" ]] || die "no $binary release found for $TRIPLE (repo=$REPO version=$VERSION)"

    if command -v "$binary" >/dev/null; then
        say "$("$binary" --version 2>/dev/null || echo "$binary") found — updating"
    else
        say "installing $binary"
    fi

    curl -fL --progress-bar ${TOKEN:+--oauth2-bearer "$TOKEN"} \
        -H "Accept: application/octet-stream" -o "$WORK/$binary.tar.gz" "$url"
    # Extract into a per-binary dir and find the executable, so this works whether
    # the tarball holds the binary at its root or under a versioned subdirectory.
    local dest="$WORK/$binary.d"
    mkdir -p "$dest"
    tar -xzf "$WORK/$binary.tar.gz" -C "$dest"
    local extracted
    extracted="$(find "$dest" -type f -name "$binary" | head -n1)"
    [[ -n "$extracted" ]] || die "could not find $binary in the downloaded archive"
    install -m 0755 "$extracted" "$bin_path"
    say "installed $("$bin_path" --version 2>/dev/null || echo "$binary") -> $bin_path"
}

# Make sure $INSTALL_DIR is on PATH for this run, and persist it to the shell rc if missing.
ensure_on_path() {
    case ":$PATH:" in *":$INSTALL_DIR:"*) return ;; esac
    export PATH="$INSTALL_DIR:$PATH"

    local rc line marker="# added by lore install.sh"
    case "$(basename "${SHELL:-}")" in
        zsh)  rc="$HOME/.zshrc"                   ; line="export PATH=\"$INSTALL_DIR:\$PATH\"  $marker" ;;
        bash) rc="$HOME/.bashrc"                  ; line="export PATH=\"$INSTALL_DIR:\$PATH\"  $marker" ;;
        fish) rc="$HOME/.config/fish/config.fish" ; line="fish_add_path \"$INSTALL_DIR\"  $marker" ;;
        *)    rc="$HOME/.profile"                 ; line="export PATH=\"$INSTALL_DIR:\$PATH\"  $marker" ;;
    esac
    mkdir -p "$(dirname "$rc")"
    # Dedup on $INSTALL_DIR (not the marker) so a different --install-dir still persists.
    grep -qsF "$INSTALL_DIR" "$rc" || printf '\n%s\n' "$line" >> "$rc"
    say "added $INSTALL_DIR to PATH in $rc — restart your shell to pick it up"
}

# Install loreserver, launch it on its zero-config ports, and print what to try next.
run_demo() {
    install_binary loreserver

    # Send server logs to a file instead of the terminal — otherwise the periodic
    # store/memory stats (logged every 10s) bury the banner below. Default to a
    # filter that drops just that spam.
    local logdir="${TMPDIR:-/tmp}"
    local log="${logdir%/}/loreserver-demo.log"
    RUST_LOG="${RUST_LOG:-info}" \
        "$INSTALL_DIR/loreserver" >"$log" 2>&1 &
    local pid=$!
    trap 'kill "$pid" 2>/dev/null || true; exit 0' INT TERM

    local i ready=0
    for ((i = 0; i < 20; i++)); do
        if curl -fsS "http://127.0.0.1:$HTTP_PORT/health_check" >/dev/null 2>&1; then ready=1; break; fi
        kill -0 "$pid" 2>/dev/null || break   # server exited early; stop waiting
        sleep 0.5
    done

    if [[ "$ready" != 1 ]]; then
        say "loreserver did not come up — last lines from $log:"
        tail -n 20 "$log" >&2 || true
        kill "$pid" 2>/dev/null || true
        return 1
    fi

    cat >&2 <<EOF

loreserver is running:
    gRPC/QUIC : lore://127.0.0.1:$GRPC_PORT
    HTTP      : http://127.0.0.1:$HTTP_PORT   (health: /health_check)
    logs      : $log   (run: tail -f $log)

Open a NEW terminal and try:
    curl -i http://127.0.0.1:$HTTP_PORT/health_check
    mkdir ~/my-project && cd ~/my-project
    lore repository create lore://127.0.0.1:$GRPC_PORT/my-project

Then continue the quickstart to add, commit, and push:
    https://epicgames.github.io/lore/tutorials/quickstart/

(Ctrl-C to stop the server)
EOF

    wait "$pid"
}

mkdir -p "$INSTALL_DIR"

# Fetch release metadata once; every install_binary call greps it. Surface the
# real curl error (404/403/network) here instead of masking it as the later
# friendly per-binary "no … release found", which means something different.
if ! RELEASE_JSON="$(fetch_release 2>&1)"; then
    die "could not fetch $VERSION release for $REPO:
$RELEASE_JSON
hint: for a private repo set GITHUB_TOKEN or run 'gh auth login'"
fi

if [[ "$DEMO" == 1 ]]; then
    PRIOR_LORE="$(command -v lore || true)"
    install_binary lore
    ensure_on_path
    if [[ -n "$PRIOR_LORE" && "$PRIOR_LORE" != "$INSTALL_DIR/lore" ]]; then
        say "note: another 'lore' is at $PRIOR_LORE — ensure $INSTALL_DIR comes first on PATH"
    fi
    run_demo
elif [[ "$SERVER" == 1 ]]; then
    install_binary loreserver
    ensure_on_path
    say ""
    say "Done. \`loreserver\` unpacked in $INSTALL_DIR."
else
    PRIOR_LORE="$(command -v lore || true)"
    install_binary lore
    ensure_on_path
    if [[ -n "$PRIOR_LORE" && "$PRIOR_LORE" != "$INSTALL_DIR/lore" ]]; then
        say "note: another 'lore' is at $PRIOR_LORE — ensure $INSTALL_DIR comes first on PATH"
    fi
    say ""
    say "Done. Run 'lore --version', or re-run with --demo to launch a local server."
fi
