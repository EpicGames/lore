#!/usr/bin/env bash
# Runs one test over and over under contention: LOOPS (default 6) concurrent repeat-test.sh loops
# of <runs> runs per test binary, every binary's loops at once. Given a fixed binary and one built
# before the fix, the base's failures show the contention the fix ran under.
# Usage: contention.sh <test name> <runs> <out dir> <test binary>...
# Run it from the test's crate directory. Loop <l> of binary <k> writes <out>/<k>-<l>/;
# <out>/result has one line per binary: its failures of its runs.
set -u
here=$(cd "$(dirname "$0")" && pwd)
name=$1 runs=$2 out=$3
shift 3
k=0
for bin in "$@"; do
    k=$((k + 1))
    for loop in $(seq 1 "${LOOPS:-6}"); do
        "$here/repeat-test.sh" "$bin" "$name" "$runs" "$out/$k-$loop" &
    done
done
wait
k=0
for bin in "$@"; do
    k=$((k + 1))
    failed=$(cat "$out/$k"-*/summary.log | grep -c 'exit=[1-9]')
    total=$(cat "$out/$k"-*/summary.log | grep -c '^run ')
    echo "$bin: $failed failures of $total runs"
done > "$out/result"
