#!/usr/bin/env bash
# The verification set for a change on the milestone 3 line: clippy and rustfmt on the FUSE
# crates, the FUSE, State and command suites, the lore-fuse and lore-io tests, the remote FUSE
# integration tests, a debug client and server build, and the FUSE and service pytest suites.
# Usage: verify.sh <tag>
# Each step logs to M3_VERIFY_OUT/verify-<tag>-<step>-<time>.log (default /tmp/lore-fuse-m3/verify).
# Prints the summary file's path; it holds one "<step> exit=<status>" line per step, then "done".
set -u
root=$(cd "$(dirname "$0")/../../.." && pwd)
out=${M3_VERIFY_OUT:-/tmp/lore-fuse-m3/verify}
mkdir -p "$out"
cd "$root" || exit 1
ts=$(date +%H%M%S)
prefix=$out/verify-$1
summary=$prefix-summary-$ts.log

step() {
    local name=$1
    shift
    "$@" > "$prefix-$name-$ts.log" 2>&1
    echo "$name exit=$?" >> "$summary"
}

crates=(-p lore-revision -p lore-fuse -p lore-io -p lore-integration-tests)
# double_must_use and deprecated: toolchain 1.99.0 (2026-10-02) added them, and they fire across
# the workspace outside milestone 3 code.
step clippy cargo clippy "${crates[@]}" --all-targets -- -D warnings
step fmt cargo fmt "${crates[@]}" --check
step suites cargo test -p lore-revision --test fuse_allocations --test fuse_lookup_latency --test fuse_mount \
    --test state --test node_modify
step unit cargo test -p lore-revision --test unit
step commands cargo test -p lore-revision --test stage --test stage_intent --test unstage --test commit \
    --test dirty --test diff_views --test filesystem_diff_stream --test diff3_streaming --test realize_views \
    --test helper --test reset --test nested_repository --test merge_metadata_inherit \
    --test sync --test path_allocations --test parallel_reads --test sync_layer_views --test view_change_exactness
step protocol cargo test -p lore-fuse --test protocol_bench --test mount_read_write
step fuselib cargo test -p lore-fuse --test unit
step io cargo test -p lore-io
step remote cargo test -p lore-integration-tests --features integration_tests --test integration fuse_remote
step build cargo build -p lore-client -p lore-server
step pytest uv run pytest scripts/test/test_fuse.py scripts/test/test_service.py \
    --lore-client-binary=debug --lore-server-binary=debug -p no:cacheprovider
echo "done" >> "$summary"
echo "$summary"
