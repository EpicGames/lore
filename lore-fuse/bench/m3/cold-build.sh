#!/usr/bin/env bash
# One cold virtual build, for quiet-run.sh: cold-vfs.sh clones BENCH_REV with the recording's
# prefetch, then build.sh builds M3_TARGET in the clone, without a quiet wait of its own.
# Usage: cold-build.sh <label>
# Environment: M3_VIEW (default /tmp/lore-fuse-m2/view-linux.txt), M3_RECORDING (default
# /tmp/lore-fuse-m2/recordings/scw-build-hints.txt; set and empty for a clone with no recording),
# M3_TARGET (default ShaderCompileWorker), M3_NET (default sdwan0), the interface whose received
# bytes are sampled.
# Writes service-, clone-, env- (the service's LORE_CACHE_GETS) and net-<label>.log to M2_OUT
# beside build.sh's run directory. Prints
#   <label> clone_s=<s> [gpf_s=<s>] build_s=<s> [link_failed=ignored] total_s=<s> exit=<status> KEEP|DISCARD
# with gpf_s, the project file generation, when build.sh generates them (M2_GENERATE_PROJECT_FILES),
# and link_failed=ignored when build.sh counted a link-only failure as succeeded
# (M2_IGNORE_LINK_FAILURE), and exits with the build's status.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"

label=$1
view=${M3_VIEW:-/tmp/lore-fuse-m2/view-linux.txt}
recording=${M3_RECORDING-/tmp/lore-fuse-m2/recordings/scw-build-hints.txt}
target=${M3_TARGET:-ShaderCompileWorker}
net=${M3_NET:-sdwan0}
mkdir -p "$M2_OUT" "$(dirname "$M3_MOUNT")"

# One line a second: time and received bytes, for the link rate the run saw.
( while :; do echo "$(date +%s.%N) $(cat "/sys/class/net/$net/statistics/rx_bytes")"; sleep 1; done ) \
    > "$M2_OUT/net-$label.log" 2>&1 &
sampler=$!
trap 'kill "$sampler" 2> /dev/null' EXIT

clone=$(M2_SERVICE_LOG=$M2_OUT/service-$label.log \
    "$here/../m2/cold-vfs.sh" "$view" "$M3_MOUNT" "$recording" 2> "$M2_OUT/clone-$label.log") || exit 2
for pid in $(m2_service_pids); do
    echo "$pid $(tr '\0' '\n' < "/proc/$pid/environ" | grep '^LORE_CACHE_GETS=' || echo 'LORE_CACHE_GETS unset')"
done > "$M2_OUT/env-$label.log" 2>&1

output=$(M2_SKIP_QUIET=1 "$here/../m2/build.sh" "$M3_MOUNT" "$target" "$label")
read -r _ _ seconds status verdict <<< "$(tail -2 <<< "$output" | head -1)"
steps=$(tail -1 <<< "$output")/steps.log
gpf=$(sed -n 's/^gpf_s=\([0-9.]*\) .*/\1/p' "$steps" 2> /dev/null)
link=
grep -q link_failure_ignored "$steps" 2> /dev/null && link="link_failed=ignored "
echo "$label clone_s=$clone ${gpf:+gpf_s=$gpf }build_s=$seconds ${link}total_s=$(echo "$clone + ${gpf:-0} + $seconds" | bc) exit=$status $verdict"
"$LORE" service stop --force > /dev/null 2>&1
exit "${status:-1}"
