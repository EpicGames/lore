#!/usr/bin/env bash
# Cold builds of a base and a new binary, alternated so both see the same link, each through
# quiet-run.sh; stops at the first kept run that fails, an expired login say, rather than repeating.
# Usage: compare.sh <base binary> <new binary> [pairs, default 3] [tag, default cmp]
# The summary lines are in M2_OUT/<tag>-run-<n>.out; M2_OUT/compare-<tag>.log has one line per run.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
source "$here/common.sh"

base=$1 new=$2 pairs=${3:-3} tag=${4:-cmp}
log=$M2_OUT/compare-$tag.log
mkdir -p "$M2_OUT"
for i in $(seq 1 "$pairs"); do
    for side in base new; do
        if [ "$side" = base ]; then bin=$base; else bin=$new; fi
        m3_quiet_cold_build "$tag" "$bin" "$tag-$side-$i"
        status=$?
        echo "$(date +%T) $side run $i exit $status" >> "$log"
        [ "$status" -eq 0 ] || exit 1
    done
done
echo "$(date +%T) done" >> "$log"
