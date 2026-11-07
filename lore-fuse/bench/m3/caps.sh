#!/usr/bin/env bash
# Cold builds of one binary at several cache-get caps (LORE_CACHE_GETS in the service's
# environment), rotated each round so every cap runs in every position, each through
# quiet-run.sh; stops at the first kept run that fails. CAPS lists the caps (default "64 128 0",
# 0 for no cap).
# Usage: caps.sh <binary> [rounds, default 3] [tag, default caps]
# The summary lines are in M2_OUT/<tag>-run-<n>.out; M2_OUT/caps-<tag>.log has one line per run.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"

bin=$1 rounds=${2:-3} tag=${3:-caps}
read -r -a caps <<< "${CAPS:-64 128 0}"
log=$M2_OUT/caps-$tag.log
mkdir -p "$M2_OUT"
for i in $(seq 1 "$rounds"); do
    for k in $(seq 0 $((${#caps[@]} - 1))); do
        cap=${caps[$(((i - 1 + k) % ${#caps[@]}))]}
        m3_quiet_cold_build "$tag" "$bin" "$tag-$cap-$i" LORE_CACHE_GETS="$cap"
        status=$?
        echo "$(date +%T) cap $cap round $i exit $status" >> "$log"
        [ "$status" -eq 0 ] || exit 1
    done
done
echo "$(date +%T) done" >> "$log"
