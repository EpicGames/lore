#!/bin/bash
# LOOKUP latency under concurrent writes and during a sync (M3 task 14), with the budgets asserted,
# and the ignored task 14 diagnostics:
# builds the release test binary, waits for a quiet machine (common.sh), then runs the tests pinned
# to M2_CORES. A foreign process seen during the run marks it DISCARD.
#
# Usage: latency.sh [label]
set -uo pipefail
# The build environment common.sh sources is not needed here.
M2_ENV=${M2_ENV:-/dev/null}
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"

root=$(cd "$(dirname "$0")/../../.." && pwd)
out="$M2_OUT/latency-${1:-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out"
(cd "$root" && cargo test --release -p lore-revision --test fuse_lookup_latency --no-run) \
    > "$out/build.log" 2>&1 || { echo "build failed: $out/build.log"; exit 1; }
binary=$(sed -n 's/.*Executable .*(\(.*\))$/\1/p' "$out/build.log" | tail -1)
# Cargo prints the path relative to the workspace, or absolute when the target directory is outside it.
case "$binary" in
    /*) ;;
    ?*) binary="$root/$binary" ;;
esac
[ -x "$binary" ] || { echo "no test binary in $out/build.log"; exit 1; }

m2_wait_quiet "$out/wait.log"
monitor=$(m2_start_intruder_monitor "$out/intruders.log")
echo "load before: $(cut -d' ' -f1-3 /proc/loadavg)" > "$out/run.log"
(cd "$root/lore-revision" \
    && LORE_QUIET_MACHINE=1 taskset -c "$M2_CORES" "$binary" --nocapture --test-threads=1 --include-ignored \
        warm_lookups_) >> "$out/run.log" 2>&1
status=$?
kill "$monitor"
echo "load after: $(cut -d' ' -f1-3 /proc/loadavg)" >> "$out/run.log"
if [ -s "$out/intruders.log" ]; then
    echo "DISCARD: a foreign process ran during the measurement" >> "$out/run.log"
fi
echo "$out/run.log"
exit $status
