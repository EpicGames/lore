#!/bin/bash
# Integration-test lore-drive with hurl (https://hurl.dev).
#
#   lore-drive/tests/hurl/run.sh [PORT]
#
# Creates a FRESH scratch workspace, starts target/debug/lore-drive in it,
# runs drive.hurl (fixtures/) then replace.hurl (fixtures2/ — same relative
# path, different content), and tears everything down. Requires `hurl` on
# PATH (sandbox note: `cargo install hurl@7.1.0` after
# `apt-get install -y libssl-dev pkg-config libxml2-dev libclang-dev clang`;
# hurl ≥ 8 needs rustc ≥ 1.95 which apt does not ship yet).
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
DRIVE="$REPO_ROOT/target/debug/lore-drive"
LORE="$REPO_ROOT/target/debug/lore"
PORT="${1:-8090}"
BASE="http://localhost:$PORT"
WS="$(mktemp -d /tmp/lore-hurl-ws.XXXXXX)"

[ -x "$DRIVE" ] || { echo "build first: cargo build -p lore-drive" >&2; exit 1; }
[ -x "$LORE" ]  || { echo "build first: cargo build -p lore-client" >&2; exit 1; }
command -v hurl > /dev/null || { echo "hurl not found on PATH" >&2; exit 1; }

cleanup() {
  [ -n "${DRIVE_PID:-}" ] && kill "$DRIVE_PID" 2> /dev/null || true
  rm -rf "$WS"
}
trap cleanup EXIT

( cd "$WS" && "$LORE" repository create --offline hurlrepo > /dev/null )
( cd "$WS" && exec "$DRIVE" --port "$PORT" ) > "$WS/drive.log" 2>&1 &
DRIVE_PID=$!

for _ in $(seq 1 50); do
  curl -sf "$BASE/api/v1/info" > /dev/null 2>&1 && break
  sleep 0.2
done
curl -sf "$BASE/api/v1/info" > /dev/null || { echo "lore-drive did not come up"; cat "$WS/drive.log"; exit 1; }

hurl --variable "base=$BASE" --file-root "$HERE/fixtures"  --test "$HERE/drive.hurl"
hurl --variable "base=$BASE" --file-root "$HERE/fixtures2" --test "$HERE/replace.hurl"

echo "hurl integration suite: ALL GREEN"
